/**
 * The publish lifecycle scanner (ADR 0114 D10, R36; ADR 0034's lesson).
 *
 * Publishing has three side-effectful steps after the intent is recorded: pin
 * the checkpoint and flip the spec, write the rendered markdown as an artifact
 * version, and hand the session over to ticketize. Two of the three need the
 * sandbox, so none of them may live in the request handler: an evicted session,
 * a pod restart, or a host roll must not lose a publish that a person already
 * asked for.
 *
 * The shape follows `crates/engram-coordinator/src/queue_scanner.rs`:
 * `runSpecPublishTick` is the pure step that tests drive directly, and
 * `SpecPublishScanner` is the thin timer wrapper. The route also calls
 * `runOnce` after it records the intent — the push wake that the Rust loop gets
 * from its `Notify`, so a publish starts in the same second the person clicks.
 *
 * Every step is idempotent, and each one advances the row only from its own
 * state, so the machine is exactly-once however many drivers run it.
 */

import type { Logger } from "pino";

import type { SpecCheckpointRecord, SpecCheckpointStore } from "./checkpoints.ts";
import type { SpecCheckpointService } from "./checkpoints.ts";
import type { SpecPublishState } from "../db/schema.ts";
import type { SpecPublishStore, SpecPublishWork } from "./publish.ts";

/** The label the pinned checkpoint carries in the history rail (R13). */
export const PUBLISHED_CHECKPOINT_LABEL = "Published version";

export const PUBLISH_CHECKPOINT_REASON = "publish";

export interface SpecPublishScannerConfig {
  intervalMs: number;
  batchSize: number;
  /** How long a claimed row waits before another driver may retry it. */
  retryDelayMs: number;
}

export const DEFAULT_SPEC_PUBLISH_SCANNER_CONFIG: SpecPublishScannerConfig = {
  intervalMs: 5_000,
  batchSize: 20,
  retryDelayMs: 15_000,
};

/**
 * Records the rendered markdown as an ordinary artifact version, so sharing,
 * serving and authorization come from ADR 0026 for free (ADR 0114 D10).
 */
export interface SpecPublishArtifactPublisher {
  /** The version already recorded at this artifact id, or null. */
  read(artifactId: string): Promise<{ version: number } | null>;
  publish(input: {
    artifactId: string;
    specId: string;
    sessionId: string;
    checkpointId: string;
    ownerUserId: string | null;
    title: string;
    markdown: string;
  }): Promise<{ version: number }>;
}

/** Moves the drafting session on to ticketize (R36). */
export interface SpecTicketizeHandoff {
  start(input: {
    specId: string;
    sessionId: string;
    /** A stable id, so a replayed step attaches instead of prompting twice. */
    promptId: string;
    openQuestionCount: number;
  }): Promise<void>;
}

export interface SpecPublishTickDeps {
  store: SpecPublishStore;
  checkpoints: Pick<SpecCheckpointService, "createCheckpoint">;
  checkpointStore: Pick<SpecCheckpointStore, "readCheckpoint">;
  artifacts: SpecPublishArtifactPublisher;
  ticketize: SpecTicketizeHandoff;
  config: Pick<SpecPublishScannerConfig, "batchSize" | "retryDelayMs">;
  now: () => Date;
  log: Logger;
  /** Restricts the sweep to one spec, for the route's push wake. */
  specId?: string;
}

export interface SpecPublishTickResult {
  claimed: number;
  pinned: number;
  artifactsPublished: number;
  completed: number;
  failed: number;
}

/** The stable ticket for the ticketize hand-off of one publish. */
export function ticketizePromptId(specId: string): string {
  return `spec-publish:${specId}`;
}

/** One sweep. Errors are counted per row; one bad publish never stops another. */
export async function runSpecPublishTick(
  deps: SpecPublishTickDeps,
): Promise<SpecPublishTickResult> {
  const now = deps.now();
  const result: SpecPublishTickResult = {
    claimed: 0,
    pinned: 0,
    artifactsPublished: 0,
    completed: 0,
    failed: 0,
  };
  const rows = await deps.store.claimDue({
    now,
    retryAt: new Date(now.getTime() + deps.config.retryDelayMs),
    limit: deps.config.batchSize,
    ...(deps.specId === undefined ? {} : { specId: deps.specId }),
  });
  result.claimed = rows.length;

  for (const row of rows) {
    let state = row.state;
    try {
      // Each step is one transition, so a publish that becomes ready mid-sweep
      // finishes in this sweep instead of waiting for the next tick.
      while (state !== "complete") {
        const next = await advance(deps, row, state);
        if (next === state) break;
        if (next === "pinned") result.pinned += 1;
        if (next === "artifact_published") result.artifactsPublished += 1;
        if (next === "complete") result.completed += 1;
        state = next;
      }
    } catch (error) {
      result.failed += 1;
      const message = error instanceof Error ? error.message : String(error);
      deps.log.warn(
        { specId: row.specId, state, attempts: row.attempts, error: message },
        "spec publish step failed; the scanner will retry it",
      );
      await deps.store
        .recordFailure({
          specId: row.specId,
          error: message,
          retryAt: new Date(deps.now().getTime() + deps.config.retryDelayMs),
        })
        .catch((cause: unknown) => {
          deps.log.warn(
            { specId: row.specId, error: String(cause) },
            "spec publish failure was not recorded",
          );
        });
    }
  }
  return result;
}

/** Run one transition. Returns the state that now holds. */
async function advance(
  deps: SpecPublishTickDeps,
  row: SpecPublishWork,
  state: SpecPublishState,
): Promise<SpecPublishState> {
  switch (state) {
    case "requested": {
      // The checkpoint id came with the request, and the insert keeps the first
      // writer's row, so a replay pins exactly one version.
      await deps.checkpoints.createCheckpoint(row.specId, {
        id: row.checkpointId,
        reason: PUBLISH_CHECKPOINT_REASON,
        label: PUBLISHED_CHECKPOINT_LABEL,
        authorUserId: row.requestedBy,
      });
      const pinned = await deps.store.markPinned({
        specId: row.specId,
        checkpointId: row.checkpointId,
        publishedBy: row.requestedBy,
        at: deps.now(),
      });
      // Another driver pinned it first. Its transaction is the truth.
      return pinned ? "pinned" : await currentState(deps, row);
    }
    case "pinned": {
      const existing = await deps.artifacts.read(row.artifactId);
      const version =
        existing ??
        (await deps.artifacts.publish({
          artifactId: row.artifactId,
          specId: row.specId,
          sessionId: row.sessionId,
          checkpointId: row.checkpointId,
          ownerUserId: row.ownerUserId,
          title: row.specTitle,
          markdown: await pinnedMarkdown(deps, row),
        }));
      const recorded = await deps.store.markArtifactPublished({
        specId: row.specId,
        version: version.version,
      });
      return recorded ? "artifact_published" : await currentState(deps, row);
    }
    case "artifact_published": {
      await deps.ticketize.start({
        specId: row.specId,
        sessionId: row.sessionId,
        promptId: ticketizePromptId(row.specId),
        openQuestionCount: row.acknowledgedQuestionCount,
      });
      const completed = await deps.store.markComplete({ specId: row.specId, at: deps.now() });
      return completed ? "complete" : await currentState(deps, row);
    }
    case "complete":
      return "complete";
  }
}

/** The artifact carries exactly what the pinned checkpoint holds. */
async function pinnedMarkdown(deps: SpecPublishTickDeps, row: SpecPublishWork): Promise<string> {
  const checkpoint: SpecCheckpointRecord | null = await deps.checkpointStore.readCheckpoint(
    row.specId,
    row.checkpointId,
  );
  if (!checkpoint) {
    throw new Error(`The pinned checkpoint ${row.checkpointId} for spec ${row.specId} is missing.`);
  }
  return checkpoint.renderedMarkdown;
}

async function currentState(
  deps: SpecPublishTickDeps,
  row: SpecPublishWork,
): Promise<SpecPublishState> {
  const stored = await deps.store.readPublish(row.specId);
  return stored?.state ?? "complete";
}

export interface SpecPublishScannerDeps extends Omit<SpecPublishTickDeps, "config" | "specId"> {
  config: SpecPublishScannerConfig;
  setInterval?: (callback: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

/**
 * The timer half. It owns no logic: it calls the step, logs a failure, and
 * keeps one sweep in flight at a time.
 */
export class SpecPublishScanner {
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<SpecPublishTickResult> | null = null;

  constructor(private readonly deps: SpecPublishScannerDeps) {}

  /** The full sweep. One is in flight at a time. */
  runOnce(): Promise<SpecPublishTickResult> {
    this.#tick ??= runSpecPublishTick({ ...this.deps }).finally(() => {
      this.#tick = null;
    });
    return this.#tick;
  }

  /**
   * The push wake for one spec, called by the route after it records the
   * intent. It claims only that spec's row, so it neither waits for the full
   * sweep nor takes another spec's turn.
   */
  wake(specId: string): Promise<SpecPublishTickResult> {
    return runSpecPublishTick({ ...this.deps, specId });
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.deps.setInterval ?? setInterval;
    // The callback must observe its own rejection, or a failing tick becomes an
    // unhandled rejection and the loop degrades silently.
    this.#timer = schedule(
      () =>
        void this.runOnce().catch((error: unknown) => {
          this.deps.log.warn({ error: String(error) }, "spec publish sweep failed");
        }),
      this.deps.config.intervalMs,
    );
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#tick?.catch(() => {});
  }
}
