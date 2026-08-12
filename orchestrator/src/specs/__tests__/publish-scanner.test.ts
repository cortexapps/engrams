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
import type { SpecPublishRecord, SpecPublishStore, SpecPublishWork } from "../publish.ts";

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
    if (this.row.state === "complete") return [];
    if (this.row.nextAttemptAt > input.now) return [];
    if (input.specId !== undefined && input.specId !== this.row.specId) return [];
    this.claims += 1;
    this.row = { ...this.row, attempts: this.row.attempts + 1, nextAttemptAt: input.retryAt };
    return [{ ...this.row, specTitle: "Org sandbox quotas", ownerUserId: OWNER }];
  }

  async markPinned(input: {
    specId: string;
    checkpointId: string;
    publishedBy: string | null;
    at: Date;
  }): Promise<boolean> {
    if (this.row.state !== "requested" || this.lifecycle !== "draft") return false;
    this.row = { ...this.row, state: "pinned", pinnedAt: input.at, lastError: null };
    this.lifecycle = "published";
    this.publishedCheckpointId = input.checkpointId;
    this.publishedBy = input.publishedBy;
    this.publishedAt = input.at;
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

/** Checkpoints keyed by id, so a replay at the same id inserts nothing new. */
class MemoryCheckpoints {
  readonly rows = new Map<string, SpecCheckpointRecord>();
  compactions = 0;

  async createCheckpoint(
    specId: string,
    options: CreateCheckpointOptions,
  ): Promise<SpecCheckpointRecord> {
    this.compactions += 1;
    const id = options.id ?? `mint-${this.rows.size + 1}`;
    const existing = this.rows.get(id);
    if (existing) return existing;
    const created: SpecCheckpointRecord = {
      id,
      specId,
      state: new Uint8Array([1]),
      stateVector: new Uint8Array([2]),
      renderedMarkdown: "# Org sandbox quotas\n\nA counter row per org.\n",
      docSeq: 12n,
      label: options.label,
      authorUserId: options.authorUserId ?? null,
      reason: options.reason,
      createdAt: NOW,
    };
    this.rows.set(id, created);
    return created;
  }

  async readCheckpoint(_specId: string, checkpointId: string): Promise<SpecCheckpointRecord | null> {
    return this.rows.get(checkpointId) ?? null;
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
  const checkpoints = new MemoryCheckpoints();
  const artifacts = new MemoryArtifacts();
  const ticketize = new MemoryTicketize();
  let clock = NOW.getTime();
  const deps: SpecPublishTickDeps = {
    store,
    checkpoints,
    checkpointStore: checkpoints,
    artifacts,
    ticketize,
    config: { batchSize: 10, retryDelayMs: 15_000 },
    now: () => new Date(clock),
    log,
  };
  return {
    deps,
    store,
    checkpoints,
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
      failed: 0,
    });
    expect(f.store.row.state).toBe("complete");
    expect(f.store.lifecycle).toBe("published");
    expect(f.store.publishedCheckpointId).toBe(CHECKPOINT_ID);
    expect(f.store.publishedBy).toBe(OWNER);
    expect(f.store.publishedAt).toEqual(NOW);
    expect([...f.checkpoints.rows.keys()]).toEqual([CHECKPOINT_ID]);
    expect(f.checkpoints.rows.get(CHECKPOINT_ID)?.label).toBe(PUBLISHED_CHECKPOINT_LABEL);
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
    expect(f.checkpoints.rows.size).toBe(1);
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
      failed: 0,
    });
    expect(f.checkpoints.rows.size).toBe(1);
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
    f.checkpoints.rows.set(CHECKPOINT_ID, {
      id: CHECKPOINT_ID,
      specId: SPEC_ID,
      state: new Uint8Array([1]),
      stateVector: new Uint8Array([2]),
      renderedMarkdown: "# pinned\n",
      docSeq: 12n,
      label: PUBLISHED_CHECKPOINT_LABEL,
      authorUserId: OWNER,
      reason: "publish",
      createdAt: NOW,
    });
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
