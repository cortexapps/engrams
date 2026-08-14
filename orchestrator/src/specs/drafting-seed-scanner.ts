/**
 * Durable delivery of the first drafting turn.
 *
 * Starting drafting records the phase change and this delivery intent in one
 * transaction. A scanner owns the control-plane work, so a request failure or
 * orchestrator restart cannot strand a drafting spec without its seed turn.
 * Replays use one prompt id; SendPrompt is idempotent on that id.
 */

import type { Logger } from "pino";
import type { Pool, PoolClient } from "pg";

import { sessions as defaultSessions } from "../control-plane/client.ts";
import type { SpecMessageClient } from "../routes/spec-messages.ts";
import { productionSpecProjection } from "./projection.ts";

export type SpecPhase = "ideation" | "drafting" | "published";

export interface StartDraftingResult {
  phase: SpecPhase;
  started: boolean;
  sessionId: string | null;
  promptId: string | null;
}

export interface StartDraftingInput {
  specId: string;
  at: Date;
  text: string;
}

export interface DraftingSeedWork {
  specId: string;
  sessionId: string;
  promptId: string;
  text: string;
  attempts: number;
}

export interface SpecDraftingSeedStore {
  start(input: StartDraftingInput): Promise<StartDraftingResult | null>;
  claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<DraftingSeedWork[]>;
  markDelivered(input: { specId: string; at: Date }): Promise<boolean>;
  recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void>;
}

interface SpecPhaseRow {
  phase: string;
  session_id: string | null;
}

interface DraftingSeedRow {
  spec_id: string;
  session_id: string;
  prompt_id: string;
  text: string;
  attempts: number;
}

export function draftingSeedPromptId(specId: string): string {
  return `spec-start-drafting:${specId}`;
}

/** Postgres owns both the atomic transition and the scanner queue. */
export class PostgresSpecDraftingSeedStore implements SpecDraftingSeedStore {
  constructor(private readonly pool: Pool) {}

  async start(input: StartDraftingInput): Promise<StartDraftingResult | null> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const updated = await client.query<SpecPhaseRow>(
        `UPDATE spec
            SET phase = 'drafting', updated_at = $2
          WHERE id = $1 AND phase = 'ideation' AND session_id IS NOT NULL
        RETURNING phase, session_id`,
        [input.specId, input.at],
      );
      const transitioned = updated.rows[0];
      if (transitioned?.session_id) {
        const promptId = draftingSeedPromptId(input.specId);
        await client.query(
          `INSERT INTO spec_drafting_seed (
             spec_id, session_id, prompt_id, text, state, attempts,
             next_attempt_at, requested_at
           ) VALUES ($1, $2, $3, $4, 'pending', 0, $5, $5)`,
          [input.specId, transitioned.session_id, promptId, input.text, input.at],
        );
        await client.query("COMMIT");
        return {
          phase: "drafting",
          started: true,
          sessionId: transitioned.session_id,
          promptId,
        };
      }

      const current = await client.query<SpecPhaseRow>(
        "SELECT phase, session_id FROM spec WHERE id = $1",
        [input.specId],
      );
      await client.query("COMMIT");
      const row = current.rows[0];
      if (!row) return null;
      return {
        phase: parsePhase(row.phase, input.specId),
        started: false,
        sessionId: row.session_id,
        promptId: null,
      };
    } catch (error) {
      await rollback(client);
      throw error;
    } finally {
      client.release();
    }
  }

  async claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<DraftingSeedWork[]> {
    const result = await this.pool.query<DraftingSeedRow>(
      `WITH due AS (
         SELECT spec_id
           FROM spec_drafting_seed
          WHERE state = 'pending'
            AND next_attempt_at <= $1
            AND ($4::uuid IS NULL OR spec_id = $4)
          ORDER BY next_attempt_at, spec_id
          FOR UPDATE SKIP LOCKED
          LIMIT $3
       )
       UPDATE spec_drafting_seed AS seed
          SET attempts = seed.attempts + 1,
              next_attempt_at = $2,
              last_error = NULL
         FROM due
        WHERE seed.spec_id = due.spec_id
      RETURNING seed.spec_id, seed.session_id, seed.prompt_id, seed.text, seed.attempts`,
      [input.now, input.retryAt, input.limit, input.specId ?? null],
    );
    return result.rows.map((row) => ({
      specId: row.spec_id,
      sessionId: row.session_id,
      promptId: row.prompt_id,
      text: row.text,
      attempts: row.attempts,
    }));
  }

  async markDelivered(input: { specId: string; at: Date }): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_drafting_seed
          SET state = 'delivered', delivered_at = $2, last_error = NULL
        WHERE spec_id = $1 AND state = 'pending'`,
      [input.specId, input.at],
    );
    return (result.rowCount ?? 0) === 1;
  }

  async recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void> {
    await this.pool.query(
      `UPDATE spec_drafting_seed
          SET last_error = $2, next_attempt_at = $3
        WHERE spec_id = $1 AND state = 'pending'`,
      [input.specId, input.error, input.retryAt],
    );
  }
}

async function rollback(client: PoolClient): Promise<void> {
  await client.query("ROLLBACK").catch(() => undefined);
}

function parsePhase(value: string, specId: string): SpecPhase {
  if (value === "ideation" || value === "drafting" || value === "published") return value;
  throw new Error(`Spec ${specId} has an invalid phase: ${value}`);
}

export interface DraftingSeedSender {
  send(work: DraftingSeedWork): Promise<void>;
}

export function makeDraftingSeedSender(input: {
  sessions: SpecMessageClient;
  preparePrompt: (sessionId: string, status: string) => Promise<void>;
}): DraftingSeedSender {
  return {
    async send(work) {
      const session = await input.sessions.getSession({ sessionId: work.sessionId });
      await input.preparePrompt(work.sessionId, session.session?.status ?? "");
      await input.sessions.sendPrompt({
        sessionId: work.sessionId,
        promptId: work.promptId,
        text: work.text,
      });
    },
  };
}

export function productionDraftingSeedSender(): DraftingSeedSender {
  return makeDraftingSeedSender({
    sessions: defaultSessions,
    preparePrompt: (sessionId, status) => productionSpecProjection.preparePrompt(sessionId, status),
  });
}

export interface DraftingSeedScannerConfig {
  intervalMs: number;
  batchSize: number;
  retryDelayMs: number;
}

export const DEFAULT_DRAFTING_SEED_SCANNER_CONFIG: DraftingSeedScannerConfig = {
  intervalMs: 5_000,
  batchSize: 20,
  retryDelayMs: 15_000,
};

export interface DraftingSeedTickDeps {
  store: SpecDraftingSeedStore;
  sender: DraftingSeedSender;
  config: Pick<DraftingSeedScannerConfig, "batchSize" | "retryDelayMs">;
  now: () => Date;
  log: Logger;
  specId?: string;
}

export interface DraftingSeedTickResult {
  claimed: number;
  delivered: number;
  failed: number;
}

/** One independently driveable sweep. A failed row remains due for retry. */
export async function runDraftingSeedTick(
  deps: DraftingSeedTickDeps,
): Promise<DraftingSeedTickResult> {
  const now = deps.now();
  const rows = await deps.store.claimDue({
    now,
    retryAt: new Date(now.getTime() + deps.config.retryDelayMs),
    limit: deps.config.batchSize,
    ...(deps.specId === undefined ? {} : { specId: deps.specId }),
  });
  const result = { claimed: rows.length, delivered: 0, failed: 0 };
  for (const row of rows) {
    try {
      await deps.sender.send(row);
      if (await deps.store.markDelivered({ specId: row.specId, at: deps.now() })) {
        result.delivered += 1;
      }
    } catch (error) {
      result.failed += 1;
      const message = error instanceof Error ? error.message : String(error);
      deps.log.warn(
        { specId: row.specId, attempts: row.attempts, error: message },
        "drafting seed delivery failed; the scanner will retry it",
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
            "drafting seed failure was not recorded",
          );
        });
    }
  }
  return result;
}

export interface DraftingSeedScannerDeps extends Omit<DraftingSeedTickDeps, "config" | "specId"> {
  config: DraftingSeedScannerConfig;
  setInterval?: (callback: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

/** Thin timer wrapper around the driveable seed step. */
export class DraftingSeedScanner {
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<DraftingSeedTickResult> | null = null;

  constructor(private readonly deps: DraftingSeedScannerDeps) {}

  runOnce(): Promise<DraftingSeedTickResult> {
    this.#tick ??= runDraftingSeedTick({ ...this.deps }).finally(() => {
      this.#tick = null;
    });
    return this.#tick;
  }

  wake(specId: string): Promise<DraftingSeedTickResult> {
    return runDraftingSeedTick({ ...this.deps, specId });
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.deps.setInterval ?? setInterval;
    this.#timer = schedule(
      () =>
        void this.runOnce().catch((error: unknown) => {
          this.deps.log.warn({ error: String(error) }, "drafting seed sweep failed");
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
