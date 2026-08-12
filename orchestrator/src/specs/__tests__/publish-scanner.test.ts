import { describe, expect, test } from "bun:test";
import pino from "pino";

import type { SpecPublishState } from "../../db/schema.ts";
import type { CreateCheckpointOptions, SpecCheckpointRecord } from "../checkpoints.ts";
import {
  PUBLISHED_CHECKPOINT_LABEL,
  runSpecPublishTick,
  SpecPublishScanner,
  ticketizePromptId,
  type SpecPublishArtifactPublisher,
  type SpecPublishTickDeps,
  type SpecTicketizeHandoff,
} from "../publish-scanner.ts";
import type {
  PinOutcome,
  PinPublishInput,
  SpecPublishRecord,
  SpecPublishStore,
  SpecPublishWork,
  VerifyForPinResult,
} from "../publish.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000000126";
const SESSION_ID = "00000000-0000-4000-8000-000000000127";
const CHECKPOINT_ID = "00000000-0000-4000-8000-0000000001c0";
const ARTIFACT_ID = "artifact-1";
const OWNER = "user-owner";
const NOW = new Date("2026-08-12T15:04:00.000Z");

const log = pino({ enabled: false });

function record(overrides: Partial<SpecPublishRecord> = {}): SpecPublishRecord {
  return {
    specId: SPEC_ID,
    sessionId: SESSION_ID,
    checkpointId: CHECKPOINT_ID,
    artifactId: ARTIFACT_ID,
    artifactVersion: null,
    state: "requested",
    requestedBy: OWNER,
    requestedAt: NOW,
    acknowledgedQuestionCount: 0,
    acknowledgedQuestionIds: [],
    gapCheckRunId: "run-1",
    attempts: 0,
    nextAttemptAt: NOW,
    lastError: null,
    pinnedAt: null,
    completedAt: null,
    ...overrides,
  };
}

/** The store, with the state machine's guards but no Postgres. */
class MemoryPublishStore implements SpecPublishStore {
  row: SpecPublishRecord;
  lifecycle: "draft" | "published" = "draft";
  publishedCheckpointId: string | null = null;
  publishedBy: string | null = null;
  publishedAt: Date | null = null;
  claims = 0;
  /** The document revision, the section states and the open questions the
   *  pin transaction would read. */
  semanticDocSeq = 12n;
  sectionStates = new Map<string, "confirmed" | "drafted" | "n/a-with-reason">([
    ["sec-req", "confirmed"],
    ["sec-data", "confirmed"],
  ]);
  openQuestionIds: string[] = [];
  readonly checkpoints = new Map<string, string>();

  constructor(row: SpecPublishRecord = record()) {
    this.row = row;
  }

  async readTarget() {
    return null;
  }

  async readPublish(): Promise<SpecPublishRecord | null> {
    return this.row;
  }

  async listOpenQuestions() {
    return [];
  }

  async insertRequest(input: SpecPublishRecord): Promise<SpecPublishRecord> {
    return input;
  }

  async claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<SpecPublishWork[]> {
    if (this.row.state === "complete" || this.row.state === "blocked") return [];
    if (this.row.nextAttemptAt > input.now) return [];
    if (input.specId !== undefined && input.specId !== this.row.specId) return [];
    this.claims += 1;
    this.row = { ...this.row, attempts: this.row.attempts + 1, nextAttemptAt: input.retryAt };
    return [{ ...this.row, specTitle: "Org sandbox quotas", ownerUserId: OWNER }];
  }

  /** Mirrors the pin transaction: the same guards, in the same order. */
  async pin(input: PinPublishInput): Promise<PinOutcome> {
    if (this.row.state !== "requested" || this.lifecycle !== "draft") {
      return { kind: "not_requested" };
    }
    if (this.semanticDocSeq !== input.semanticDocSeq) {
      return { kind: "stale_document", currentSemanticDocSeq: this.semanticDocSeq };
    }
    const unsettled = input.requiredSectionIds.filter((id) => {
      const state = this.sectionStates.get(id);
      if (state === undefined) return true;
      return !(state === "confirmed" || state === "n/a-with-reason");
    });
    if (unsettled.length > 0) {
      return {
        kind: "gate_failed",
        reason: `${unsettled.length} required sections are no longer settled.`,
      };
    }
    const acknowledged = [...input.acknowledgedQuestionIds].sort();
    const open = [...this.openQuestionIds].sort();
    if (open.length !== acknowledged.length || open.some((id, i) => id !== acknowledged[i])) {
      return {
        kind: "gate_failed",
        reason: "The open questions changed after the acknowledgment.",
      };
    }
    this.checkpoints.set(input.checkpoint.id, input.checkpoint.renderedMarkdown);
    this.row = { ...this.row, state: "pinned", pinnedAt: input.at, lastError: null };
    this.lifecycle = "published";
    this.publishedCheckpointId = input.checkpoint.id;
    this.publishedBy = input.publishedBy;
    this.publishedAt = input.at;
    return { kind: "pinned" };
  }

  async markBlocked(input: { specId: string; reason: string; at: Date }): Promise<boolean> {
    if (this.row.state !== "requested") return false;
    this.row = { ...this.row, state: "blocked", lastError: input.reason, nextAttemptAt: input.at };
    return true;
  }

  async resetBlocked(input: {
    specId: string;
    requestedBy: string | null;
    requestedAt: Date;
    acknowledgedQuestionIds: readonly string[];
    gapCheckRunId: string | null;
  }): Promise<boolean> {
    if (this.row.state !== "blocked") return false;
    this.row = {
      ...this.row,
      state: "requested",
      requestedBy: input.requestedBy,
      requestedAt: input.requestedAt,
      acknowledgedQuestionIds: [...input.acknowledgedQuestionIds],
      acknowledgedQuestionCount: input.acknowledgedQuestionIds.length,
      gapCheckRunId: input.gapCheckRunId,
      attempts: 0,
      nextAttemptAt: input.requestedAt,
      lastError: null,
    };
    return true;
  }

  async markArtifactPublished(input: { specId: string; version: number }): Promise<boolean> {
    if (this.row.state !== "pinned") return false;
    this.row = {
      ...this.row,
      state: "artifact_published",
      artifactVersion: input.version,
      lastError: null,
    };
    return true;
  }

  async markComplete(input: { specId: string; at: Date }): Promise<boolean> {
    if (this.row.state !== "artifact_published") return false;
    this.row = { ...this.row, state: "complete", completedAt: input.at, lastError: null };
    return true;
  }

  async recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void> {
    this.row = { ...this.row, lastError: input.error, nextAttemptAt: input.retryAt };
  }
}

/** The compaction the pin freezes, and the checkpoint the artifact leg reads. */
class MemoryDocuments {
  compactions = 0;
  markdown = "# Org sandbox quotas\n\nA counter row per org.\n";

  constructor(private readonly store: MemoryPublishStore) {}

  async compact(specId: string) {
    this.compactions += 1;
    void specId;
    return {
      state: new Uint8Array([1]),
      stateVector: new Uint8Array([2]),
      renderedMarkdown: this.markdown,
      coveredSeq: 12n,
      coveredSemanticDocSeq: this.store.semanticDocSeq,
    };
  }
}

/** The gate, as the service would re-check it for the compacted revision. */
class MemoryGate {
  verifications = 0;
  /** Set to refuse, exactly as verifyForPin would. */
  refuseWith: { retryable: boolean; reason: string } | null = null;

  async verifyForPin(
    specId: string,
    input: { semanticDocSeq: bigint; acknowledgedQuestionIds: readonly string[] },
  ): Promise<VerifyForPinResult> {
    this.verifications += 1;
    void specId;
    void input;
    if (this.refuseWith) return { ok: false, ...this.refuseWith };
    return { ok: true, requiredSectionIds: ["sec-req", "sec-data"] };
  }
}

/** Reads the checkpoint the pin transaction wrote. */
class MemoryCheckpointStore {
  constructor(private readonly store: MemoryPublishStore) {}

  async readCheckpoint(
    specId: string,
    checkpointId: string,
  ): Promise<SpecCheckpointRecord | null> {
    const markdown = this.store.checkpoints.get(checkpointId);
    if (markdown === undefined) return null;
    return {
      id: checkpointId,
      specId,
      state: new Uint8Array([1]),
      stateVector: new Uint8Array([2]),
      renderedMarkdown: markdown,
      docSeq: 12n,
      label: PUBLISHED_CHECKPOINT_LABEL,
      authorUserId: OWNER,
      reason: "publish",
      createdAt: NOW,
    };
  }
}

class MemoryArtifacts implements SpecPublishArtifactPublisher {
  readonly versions = new Map<string, number>();
  readonly publishes: Array<{ artifactId: string; markdown: string; title: string }> = [];
  failures = 0;

  async read(artifactId: string): Promise<{ version: number } | null> {
    const version = this.versions.get(artifactId);
    return version === undefined ? null : { version };
  }

  async publish(input: {
    artifactId: string;
    markdown: string;
    title: string;
  }): Promise<{ version: number }> {
    if (this.failures > 0) {
      this.failures -= 1;
      throw new Error("the sandbox is not reachable");
    }
    this.publishes.push({
      artifactId: input.artifactId,
      markdown: input.markdown,
      title: input.title,
    });
    this.versions.set(input.artifactId, 1);
    return { version: 1 };
  }
}

class MemoryTicketize implements SpecTicketizeHandoff {
  readonly starts: Array<{ specId: string; promptId: string; openQuestionCount: number }> = [];
  failures = 0;

  async start(input: {
    specId: string;
    sessionId: string;
    promptId: string;
    openQuestionCount: number;
  }): Promise<void> {
    if (this.failures > 0) {
      this.failures -= 1;
      throw new Error("the session is evicted");
    }
    this.starts.push({
      specId: input.specId,
      promptId: input.promptId,
      openQuestionCount: input.openQuestionCount,
    });
  }
}

function fixture(row?: SpecPublishRecord) {
  const store = new MemoryPublishStore(row);
  const documents = new MemoryDocuments(store);
  const gate = new MemoryGate();
  const checkpointStore = new MemoryCheckpointStore(store);
  const artifacts = new MemoryArtifacts();
  const ticketize = new MemoryTicketize();
  let clock = NOW.getTime();
  const deps: SpecPublishTickDeps = {
    store,
    documents,
    gate,
    checkpointStore,
    artifacts,
    ticketize,
    config: { batchSize: 10, retryDelayMs: 15_000 },
    now: () => new Date(clock),
    log,
  };
  return {
    deps,
    store,
    documents,
    gate,
    artifacts,
    ticketize,
    advanceClock: (ms: number) => {
      clock += ms;
    },
  };
}

describe("spec publish scanner", () => {
  test("one sweep pins, records the artifact version, and starts ticketize (R36)", async () => {
    const f = fixture(record({ acknowledgedQuestionCount: 3 }));

    const result = await runSpecPublishTick(f.deps);

    expect(result).toEqual({
      claimed: 1,
      pinned: 1,
      artifactsPublished: 1,
      completed: 1,
      blocked: 0,
      failed: 0,
    });
    expect(f.store.row.state).toBe("complete");
    expect(f.store.lifecycle).toBe("published");
    expect(f.store.publishedCheckpointId).toBe(CHECKPOINT_ID);
    expect(f.store.publishedBy).toBe(OWNER);
    expect(f.store.publishedAt).toEqual(NOW);
    expect([...f.store.checkpoints.keys()]).toEqual([CHECKPOINT_ID]);
    expect(f.store.row.artifactVersion).toBe(1);
    expect(f.artifacts.publishes).toEqual([
      {
        artifactId: ARTIFACT_ID,
        markdown: "# Org sandbox quotas\n\nA counter row per org.\n",
        title: "Org sandbox quotas",
      },
    ]);
    expect(f.ticketize.starts).toEqual([
      { specId: SPEC_ID, promptId: ticketizePromptId(SPEC_ID), openQuestionCount: 3 },
    ]);
  });

  test("further sweeps change nothing: one checkpoint, one artifact version", async () => {
    const f = fixture();

    await runSpecPublishTick(f.deps);
    f.advanceClock(60_000);
    const second = await runSpecPublishTick(f.deps);
    f.advanceClock(60_000);
    const third = await runSpecPublishTick(f.deps);

    expect(second.claimed).toBe(0);
    expect(third.claimed).toBe(0);
    expect(f.store.checkpoints.size).toBe(1);
    expect(f.artifacts.publishes).toHaveLength(1);
    expect(f.ticketize.starts).toHaveLength(1);
  });

  test("a failed artifact leg keeps the pin and retries only that leg", async () => {
    const f = fixture();
    f.artifacts.failures = 1;

    const first = await runSpecPublishTick(f.deps);

    expect(first).toEqual({
      claimed: 1,
      pinned: 1,
      artifactsPublished: 0,
      completed: 0,
      blocked: 0,
      failed: 1,
    });
    expect(f.store.row.state).toBe("pinned");
    expect(f.store.lifecycle).toBe("published");
    expect(f.store.row.lastError).toBe("the sandbox is not reachable");

    f.advanceClock(20_000);
    const second = await runSpecPublishTick(f.deps);

    expect(second).toEqual({
      claimed: 1,
      pinned: 0,
      artifactsPublished: 1,
      completed: 1,
      blocked: 0,
      failed: 0,
    });
    expect(f.store.checkpoints.size).toBe(1);
    expect(f.artifacts.publishes).toHaveLength(1);
    expect(f.store.row.lastError).toBeNull();
  });

  test("a claim waits for its retry delay, so two drivers do not race", async () => {
    const f = fixture();
    f.artifacts.failures = 1;

    await runSpecPublishTick(f.deps);
    const tooSoon = await runSpecPublishTick(f.deps);

    expect(tooSoon.claimed).toBe(0);
    expect(f.store.claims).toBe(1);
  });

  test("an artifact recorded before a crash is adopted, not written twice", async () => {
    const f = fixture(record({ state: "pinned", pinnedAt: NOW }));
    f.store.checkpoints.set(CHECKPOINT_ID, "# pinned\n");
    f.artifacts.versions.set(ARTIFACT_ID, 1);

    const result = await runSpecPublishTick(f.deps);

    expect(result.artifactsPublished).toBe(1);
    expect(result.completed).toBe(1);
    expect(f.artifacts.publishes).toHaveLength(0);
    expect(f.store.row.artifactVersion).toBe(1);
  });

  test("a failed ticketize hand-off never repeats the artifact version", async () => {
    const f = fixture();
    f.ticketize.failures = 1;

    await runSpecPublishTick(f.deps);
    expect(f.store.row.state).toBe("artifact_published");

    f.advanceClock(20_000);
    await runSpecPublishTick(f.deps);

    expect(f.store.row.state).toBe("complete");
    expect(f.artifacts.publishes).toHaveLength(1);
    expect(f.ticketize.starts).toHaveLength(1);
  });

  test("the missing pinned checkpoint is a retry, not a wrong artifact", async () => {
    const f = fixture(record({ state: "pinned", pinnedAt: NOW }));

    const result = await runSpecPublishTick(f.deps);

    expect(result.failed).toBe(1);
    expect(f.store.row.state).toBe("pinned");
    expect(f.store.row.lastError).toContain("is missing");
    expect(f.artifacts.publishes).toHaveLength(0);
  });

  test("a required section unsettled after the request blocks the pin, not the spec", async () => {
    const f = fixture();
    // A co-editor edits the confirmed section, which flips it back to drafted.
    f.store.sectionStates.set("sec-data", "drafted");

    const result = await runSpecPublishTick(f.deps);

    expect(result).toEqual({
      claimed: 1,
      pinned: 0,
      artifactsPublished: 0,
      completed: 0,
      blocked: 1,
      failed: 0,
    });
    expect(f.store.row.state).toBe("blocked");
    expect(f.store.row.lastError).toBe("1 required sections are no longer settled.");
    // Nothing became immutable: no checkpoint, no flip, no artifact.
    expect(f.store.checkpoints.size).toBe(0);
    expect(f.store.lifecycle).toBe("draft");
    expect(f.store.publishedCheckpointId).toBeNull();
    expect(f.artifacts.publishes).toHaveLength(0);
    expect(f.ticketize.starts).toHaveLength(0);
  });

  test("a blocked publish is never claimed again by the timer", async () => {
    const f = fixture();
    f.store.sectionStates.set("sec-data", "drafted");
    await runSpecPublishTick(f.deps);

    f.advanceClock(60_000);
    const second = await runSpecPublishTick(f.deps);

    expect(second.claimed).toBe(0);
    expect(f.store.row.state).toBe("blocked");
  });

  test("a question opened after the acknowledgment blocks the pin (R35)", async () => {
    const f = fixture();
    f.store.openQuestionIds = ["q-new"];

    const result = await runSpecPublishTick(f.deps);

    expect(result.blocked).toBe(1);
    expect(f.store.row.lastError).toBe("The open questions changed after the acknowledgment.");
    expect(f.store.checkpoints.size).toBe(0);
  });

  test("the gate's own refusal blocks before anything is compacted into a version", async () => {
    const f = fixture();
    f.gate.refuseWith = { retryable: false, reason: "2 required sections are no longer settled." };

    const result = await runSpecPublishTick(f.deps);

    expect(result.blocked).toBe(1);
    expect(f.store.row.state).toBe("blocked");
    expect(f.store.checkpoints.size).toBe(0);
    expect(f.store.lifecycle).toBe("draft");
  });

  test("a document that merely moved defers the pin and retries", async () => {
    const f = fixture();
    f.gate.refuseWith = {
      retryable: true,
      reason: "The document moved while the publish was pinning.",
    };

    const first = await runSpecPublishTick(f.deps);
    expect(first).toEqual({
      claimed: 1,
      pinned: 0,
      artifactsPublished: 0,
      completed: 0,
      blocked: 0,
      failed: 0,
    });
    expect(f.store.row.state).toBe("requested");

    // The document settles, and the next claim pins it.
    f.gate.refuseWith = null;
    f.advanceClock(20_000);
    const second = await runSpecPublishTick(f.deps);

    expect(second.pinned).toBe(1);
    expect(second.completed).toBe(1);
    expect(f.store.checkpoints.size).toBe(1);
  });

  test("the pin transaction refuses a document that moved after the gate check", async () => {
    const f = fixture();
    // The gate verified revision 12; the row moves on before the transaction.
    f.gate.refuseWith = null;
    f.documents.markdown = "# stale\n";
    const compact = f.deps.documents.compact.bind(f.deps.documents);
    f.deps.documents.compact = async (specId: string) => {
      const compacted = await compact(specId);
      f.store.semanticDocSeq = compacted.coveredSemanticDocSeq + 1n;
      return compacted;
    };

    const result = await runSpecPublishTick(f.deps);

    expect(result.pinned).toBe(0);
    expect(result.blocked).toBe(0);
    expect(f.store.row.state).toBe("requested");
    expect(f.store.checkpoints.size).toBe(0);
    expect(f.store.lifecycle).toBe("draft");
  });

  test("a push wake for another spec claims nothing", async () => {
    const f = fixture();

    const result = await runSpecPublishTick({
      ...f.deps,
      specId: "00000000-0000-4000-8000-000000000999",
    });

    expect(result.claimed).toBe(0);
    expect(f.store.row.state).toBe("requested");
  });

  test("the timer wrapper runs the step once per tick and stops cleanly", async () => {
    const f = fixture();
    const ticks: Array<() => void> = [];
    let cleared = 0;
    let timerHandle!: ReturnType<typeof setInterval>;
    const scanner = new SpecPublishScanner({
      ...f.deps,
      config: { intervalMs: 5_000, batchSize: 10, retryDelayMs: 15_000 },
      setInterval: (callback) => {
        ticks.push(callback);
        return timerHandle;
      },
      clearInterval: () => {
        cleared += 1;
      },
    });

    await scanner.start();

    // The first sweep is eager, so a publish requested before boot lands.
    expect(f.store.row.state).toBe("complete");
    expect(ticks).toHaveLength(1);

    await scanner.stop();
    expect(cleared).toBe(1);
  });
});

/** Guards the forward-only shape of the machine. */
describe("publish states", () => {
  test("every state reachable from a request advances or terminates", async () => {
    const states: SpecPublishState[] = ["requested", "pinned", "artifact_published", "complete"];
    const seen: SpecPublishState[] = [];
    const f = fixture();
    for (let sweep = 0; sweep < 4; sweep += 1) {
      seen.push(f.store.row.state);
      await runSpecPublishTick(f.deps);
      f.advanceClock(20_000);
    }
    expect(seen[0]).toBe("requested");
    expect(f.store.row.state).toBe("complete");
    expect(states).toContain(f.store.row.state);
  });
});
