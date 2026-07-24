import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  makeInMemoryDbosStatusStore,
  makeInMemoryHeartbeatStore,
  makeInMemorySweepLeaseStore,
  makeInMemorySweepLedgerStore,
  type DbosStatusStore,
  type DbosWorkflowRow,
  type HeartbeatStore,
  type InMemoryDbosStatusSeed,
  type SweepLeaseStore,
} from "../../db/dbos-sweep.ts";
import { assertSweepPoliciesExhaustive } from "../policy.ts";
import {
  DEFAULT_SWEEP_CONFIG,
  runSweepTick,
  Sweeper,
  VersionHeartbeat,
  type SweepConfig,
  type SweepTickDeps,
} from "../sweeper.ts";

const NOW = new Date("2026-07-23T12:00:00.000Z");
const CURRENT_VERSION = "version-current";
const DEAD_VERSION = "version-dead";
const HOUR_MS = 60 * 60 * 1_000;
const log: Logger = pino({ level: "silent" });

function row(
  workflowUuid: string,
  overrides: Partial<InMemoryDbosStatusSeed> = {},
): InMemoryDbosStatusSeed {
  return {
    workflowUuid,
    name: "ToolExecWorkflow",
    status: "PENDING",
    applicationVersion: DEAD_VERSION,
    createdAtEpochMs: NOW.getTime() - 30 * 60_000,
    updatedAtEpochMs: NOW.getTime() - 30 * 60_000,
    recoveryAttempts: 0,
    ...overrides,
  };
}

interface Fixture {
  deps: SweepTickDeps;
  status: ReturnType<typeof makeInMemoryDbosStatusStore>;
  heartbeats: HeartbeatStore;
  lease: SweepLeaseStore;
  cancelled: string[];
}

async function fixture(
  rows: InMemoryDbosStatusSeed[] = [],
  overrides: Partial<SweepTickDeps> = {},
  config: Partial<SweepConfig> = {},
): Promise<Fixture> {
  const now = () => new Date(NOW);
  const heartbeats = makeInMemoryHeartbeatStore(now);
  await heartbeats.beat(CURRENT_VERSION, "pod-current");
  const lease = makeInMemorySweepLeaseStore(now);
  const ledger = makeInMemorySweepLedgerStore(now);
  const status = makeInMemoryDbosStatusStore(rows, now, {
    isVersionLive: async (applicationVersion, graceMs) =>
      (await heartbeats.liveVersions(graceMs)).includes(applicationVersion),
    recordSweep: (workflowUuid, workflowName) =>
      ledger.recordSweep(workflowUuid, workflowName),
  });
  const cancelled: string[] = [];
  const deps: SweepTickDeps = {
    owner: "sweeper-owner",
    appVersion: () => CURRENT_VERSION,
    config: { ...DEFAULT_SWEEP_CONFIG, ...config },
    heartbeats,
    lease,
    ledger,
    status,
    cancelWorkflow: async (workflowUuid) => {
      cancelled.push(workflowUuid);
    },
    log,
    now,
    ...overrides,
  };
  return { deps, status, heartbeats, lease, cancelled };
}

describe("runSweepTick", () => {
  test("returns without scanning when the lease is not held", async () => {
    const f = await fixture([row("wf-1")]);
    expect(await f.lease.tryAcquire("other-owner", 120_000)).toBe(true);

    expect(await runSweepTick(f.deps)).toEqual({
      leaseHeld: false,
      liveVersions: [],
      scanned: 0,
      decisions: [],
    });
    expect(f.status.inspect("wf-1")?.status).toBe("PENDING");
  });

  test("aborts when the sweeper cannot prove its own version is live", async () => {
    const f = await fixture([row("wf-1")], {
      appVersion: () => "unheartbeating-version",
    });

    expect(await runSweepTick(f.deps)).toEqual({
      leaseHeld: true,
      aborted: "self-version-not-live",
      liveVersions: [CURRENT_VERSION],
      scanned: 0,
      decisions: [],
    });

    // The abort still releases the lease.
    expect(await f.lease.tryAcquire("next-owner", 120_000)).toBe(true);
  });

  test("atomically adopts a fresh PENDING workflow and records one sweep", async () => {
    const f = await fixture([row("wf-pending")]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions).toEqual([
      {
        workflowUuid: "wf-pending",
        name: "ToolExecWorkflow",
        action: "adopted",
      },
    ]);
    expect(f.status.inspect("wf-pending")).toMatchObject({
      status: "ENQUEUED",
      queueName: "_dbos_internal_queue",
      applicationVersion: null,
    });
    expect((await f.deps.ledger.get("wf-pending"))?.sweepCount).toBe(1);
  });

  test("clears only the dead version on an ENQUEUED workflow", async () => {
    const f = await fixture([
      row("wf-enqueued", {
        status: "ENQUEUED",
        queueName: "existing-queue",
      }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("enqueued_cleared");
    expect(f.status.inspect("wf-enqueued")).toMatchObject({
      status: "ENQUEUED",
      queueName: "existing-queue",
      applicationVersion: null,
    });
  });

  test("cancels a stale adopt-policy workflow instead of adopting it", async () => {
    const f = await fixture([
      row("wf-stale", {
        createdAtEpochMs: NOW.getTime() - 2 * HOUR_MS,
      }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("cancelled_stale");
    expect(f.cancelled).toEqual(["wf-stale"]);
    expect(f.status.inspect("wf-stale")?.status).toBe("PENDING");
    expect((await f.deps.ledger.get("wf-stale"))?.sweepCount).toBe(1);
  });

  test("cancels a workflow that has reached the sweep-count cap", async () => {
    const f = await fixture([row("wf-capped")]);
    for (let count = 0; count < f.deps.config.maxSweeps; count++) {
      await f.deps.ledger.recordSweep("wf-capped", "ToolExecWorkflow");
    }

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("cancelled_capped");
    expect(f.cancelled).toEqual(["wf-capped"]);
    expect((await f.deps.ledger.get("wf-capped"))?.sweepCount).toBe(
      f.deps.config.maxSweeps,
    );
  });

  test("does not touch a suppressed workflow", async () => {
    const f = await fixture([row("wf-suppressed")]);
    await f.deps.ledger.recordSweep("wf-suppressed", "ToolExecWorkflow");
    await f.deps.ledger.setSuppressed("wf-suppressed", true);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("suppressed");
    expect(f.cancelled).toEqual([]);
    expect(f.status.inspect("wf-suppressed")?.status).toBe("PENDING");
  });

  test("reports an unknown workflow name without changing it", async () => {
    const f = await fixture([
      row("wf-unknown", { name: "DeletedWorkflowName" }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("alert_only");
    expect(f.status.inspect("wf-unknown")?.status).toBe("PENDING");
    expect(await f.deps.ledger.get("wf-unknown")).toBeNull();
  });

  test.each([
    "temp_workflow-send-message-123",
    "_dbos_debouncer_workflow",
  ])("adopts a fresh prefix-covered workflow: %s", async (name) => {
    const f = await fixture([row("wf-prefix", { name })]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("adopted");
    expect(f.status.inspect("wf-prefix")?.applicationVersion).toBeNull();
  });

  test("caps mutating actions and stops examining rows at the cap", async () => {
    const rows = [
      row("wf-unknown", {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - 3_000,
      }),
      row("wf-1", { createdAtEpochMs: NOW.getTime() - 2_000 }),
      row("wf-2", { createdAtEpochMs: NOW.getTime() - 1_000 }),
    ];
    const f = await fixture(rows, {}, { batchCap: 1 });

    const result = await runSweepTick(f.deps);

    // scanned counts examined rows only: the row after the cap is left for
    // the next tick's cursor, not silently counted as covered.
    expect(result.scanned).toBe(2);
    expect(result.decisions.map((decision) => decision.action)).toEqual([
      "alert_only",
      "adopted",
    ]);
    expect(f.status.inspect("wf-1")?.applicationVersion).toBeNull();
    expect(f.status.inspect("wf-2")?.applicationVersion).toBe(DEAD_VERSION);
  });

  test("paginates past more than one page of alert-only rows to adopt younger work", async () => {
    const alertOnlyRows = Array.from({ length: 5 }, (_, index) =>
      row(`wf-unknown-${index}`, {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - (10_000 - index),
      }),
    );
    const f = await fixture(
      [
        ...alertOnlyRows,
        row("wf-adoptable", {
          createdAtEpochMs: NOW.getTime() - 1_000,
        }),
      ],
      {},
      { batchCap: 1 },
    );

    const result = await runSweepTick(f.deps);

    expect(result.scanned).toBe(6);
    expect(result.decisions.map((decision) => decision.action)).toEqual([
      "alert_only",
      "alert_only",
      "alert_only",
      "alert_only",
      "alert_only",
      "adopted",
    ]);
    expect(f.status.inspect("wf-adoptable")?.applicationVersion).toBeNull();
  });

  test("stops pagination at the total scan budget and logs the cap", async () => {
    const alertOnlyRows = Array.from({ length: 40 }, (_, index) =>
      row(`wf-unknown-${String(index).padStart(2, "0")}`, {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - (100_000 - index),
      }),
    );
    const f = await fixture(
      [
        ...alertOnlyRows,
        row("wf-beyond-budget", {
          createdAtEpochMs: NOW.getTime() - 1_000,
        }),
      ],
      {},
      { batchCap: 1 },
    );
    const warnings: string[] = [];
    f.deps.log = {
      info() {},
      warn(_fields: object, message: string) {
        warnings.push(message);
      },
      error() {},
    } as unknown as Logger;

    const result = await runSweepTick(f.deps);

    expect(result.scanned).toBe(40);
    expect(result.decisions).toHaveLength(40);
    expect(f.status.inspect("wf-beyond-budget")?.applicationVersion).toBe(
      DEAD_VERSION,
    );
    expect(warnings).toEqual([
      "DBOS orphan sweep scan budget exhausted; resuming from the cursor next cycle",
    ]);
  });

  test("a persisted cursor rotates past non-actionable rows to reach newer work", async () => {
    // The starvation regression: enough young alert-only rows to fill the
    // whole scan budget, with the adoptable workflow sorted after them. A
    // cursor-less scan would re-examine the alert-only prefix every tick and
    // never reach it.
    const alertOnlyRows = Array.from({ length: 40 }, (_, index) =>
      row(`wf-unknown-${String(index).padStart(2, "0")}`, {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - (100_000 - index),
      }),
    );
    const scanCursor: SweepTickDeps["scanCursor"] = { value: undefined };
    const f = await fixture(
      [
        ...alertOnlyRows,
        row("wf-beyond-budget", {
          createdAtEpochMs: NOW.getTime() - 1_000,
        }),
      ],
      { scanCursor },
      { batchCap: 1 },
    );

    const first = await runSweepTick(f.deps);
    expect(first.scanned).toBe(40);
    expect(f.status.inspect("wf-beyond-budget")?.applicationVersion).toBe(
      DEAD_VERSION,
    );
    expect(scanCursor.value?.workflowUuid).toBe("wf-unknown-39");

    const second = await runSweepTick(f.deps);
    expect(
      second.decisions.map((decision) => decision.action),
    ).toEqual(["adopted"]);
    expect(
      f.status.inspect("wf-beyond-budget")?.applicationVersion,
    ).toBeNull();
    // The pass reached the end of the backlog, so the next tick restarts
    // from the oldest row.
    expect(scanCursor.value).toBeUndefined();
  });

  test("cancels an alert-only workflow past the stale window", async () => {
    const f = await fixture([
      row("wf-dead-name", {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - 49 * HOUR_MS,
      }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]).toMatchObject({
      action: "cancelled_stale",
      reason: "unregistered workflow name past the stale window",
    });
    expect(f.cancelled).toEqual(["wf-dead-name"]);
    expect((await f.deps.ledger.get("wf-dead-name"))?.sweepCount).toBe(1);
  });

  test("Sweeper.runOnce carries its own cursor between ticks", async () => {
    const alertOnlyRows = Array.from({ length: 40 }, (_, index) =>
      row(`wf-unknown-${String(index).padStart(2, "0")}`, {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - (100_000 - index),
      }),
    );
    const f = await fixture(
      [
        ...alertOnlyRows,
        row("wf-beyond-budget", {
          createdAtEpochMs: NOW.getTime() - 1_000,
        }),
      ],
      {},
      { batchCap: 1 },
    );
    const sweeper = new Sweeper(f.deps);

    await sweeper.runOnce();
    expect(f.status.inspect("wf-beyond-budget")?.applicationVersion).toBe(
      DEAD_VERSION,
    );

    await sweeper.runOnce();
    expect(
      f.status.inspect("wf-beyond-budget")?.applicationVersion,
    ).toBeNull();
  });

  test("a persistently failing stale cancel stops inflating the sweep count at the cap", async () => {
    const f = await fixture([
      row("wf-cancel-wedged", {
        createdAtEpochMs: NOW.getTime() - 2 * HOUR_MS,
      }),
    ]);
    f.deps.cancelWorkflow = async () => {
      throw new Error("cancel keeps failing");
    };

    // maxSweeps failing cancelled_stale attempts each record intent…
    for (let attempt = 0; attempt < f.deps.config.maxSweeps; attempt++) {
      const result = await runSweepTick(f.deps);
      expect(result.decisions[0]?.action).toBe("error");
    }
    expect((await f.deps.ledger.get("wf-cancel-wedged"))?.sweepCount).toBe(
      f.deps.config.maxSweeps,
    );

    // …then the cap dominates: further ticks retry the cancel via
    // cancelled_capped WITHOUT recordSweep, so the count stays bounded.
    for (let attempt = 0; attempt < 3; attempt++) {
      await runSweepTick(f.deps);
    }
    expect((await f.deps.ledger.get("wf-cancel-wedged"))?.sweepCount).toBe(
      f.deps.config.maxSweeps,
    );

    // When the cancel finally succeeds, the workflow terminates as capped.
    f.deps.cancelWorkflow = async (workflowUuid) => {
      f.cancelled.push(workflowUuid);
    };
    const final = await runSweepTick(f.deps);
    expect(final.decisions[0]?.action).toBe("cancelled_capped");
    expect(f.cancelled).toEqual(["wf-cancel-wedged"]);
  });

  test("suppression vetoes the alert-only stale cancel", async () => {
    const f = await fixture([
      row("wf-dead-name", {
        name: "DeletedWorkflowName",
        createdAtEpochMs: NOW.getTime() - 49 * HOUR_MS,
      }),
    ]);
    await f.deps.ledger.recordSweep("wf-dead-name", "DeletedWorkflowName");
    await f.deps.ledger.setSuppressed("wf-dead-name", true);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("suppressed");
    expect(f.cancelled).toEqual([]);
    expect(f.status.inspect("wf-dead-name")?.status).toBe("PENDING");
  });

  test("contains a cancel error and continues to the next row", async () => {
    const f = await fixture([
      row("wf-bad-cancel", {
        createdAtEpochMs: NOW.getTime() - 3 * HOUR_MS,
      }),
      row("wf-next", { createdAtEpochMs: NOW.getTime() - 1_000 }),
    ]);
    f.deps.cancelWorkflow = async (workflowUuid) => {
      if (workflowUuid === "wf-bad-cancel") throw new Error("cancel failed");
      f.cancelled.push(workflowUuid);
    };

    const result = await runSweepTick(f.deps);

    expect(result.decisions).toEqual([
      {
        workflowUuid: "wf-bad-cancel",
        name: "ToolExecWorkflow",
        action: "error",
        reason: "cancel failed",
      },
      {
        workflowUuid: "wf-next",
        name: "ToolExecWorkflow",
        action: "adopted",
      },
    ]);
    expect(f.status.inspect("wf-next")?.applicationVersion).toBeNull();
  });

  test("reports a PENDING compare-and-set miss as a row error", async () => {
    const underlying = makeInMemoryDbosStatusStore([row("wf-raced")]);
    const status: DbosStatusStore = {
      ...underlying,
      async adoptPendingRecording() {
        return { flipped: false };
      },
    };
    const f = await fixture([], { status });

    const result = await runSweepTick(f.deps);

    // A lost flip race is a healthy outcome (the owner is back or DBOS moved
    // the row) — reported as raced, which never reaches the ops channel.
    expect(result.decisions[0]).toMatchObject({
      action: "raced",
      reason: "owner became live or row changed",
    });
  });

  test("does not adopt when the owner heartbeats after listing but before the flip", async () => {
    const f = await fixture([]);
    const underlying = makeInMemoryDbosStatusStore(
      [row("wf-owner-returned")],
      () => new Date(NOW),
      {
        isVersionLive: async (applicationVersion, graceMs) =>
          (await f.heartbeats.liveVersions(graceMs)).includes(
            applicationVersion,
          ),
      },
    );
    f.deps.status = {
      ...underlying,
      async listNonTerminalOnVersionsNotIn(liveVersions, limit, after) {
        const listed = await underlying.listNonTerminalOnVersionsNotIn(
          liveVersions,
          limit,
          after,
        );
        await f.heartbeats.beat(DEAD_VERSION, "pod-returned");
        return listed;
      },
    };

    const result = await runSweepTick(f.deps);

    expect(result.decisions).toEqual([
      {
        workflowUuid: "wf-owner-returned",
        name: "ToolExecWorkflow",
        action: "raced",
        reason: "owner became live or row changed",
      },
    ]);
    expect(underlying.inspect("wf-owner-returned")).toMatchObject({
      status: "PENDING",
      applicationVersion: DEAD_VERSION,
    });
  });

  test("does not record a sweep when the adoption transaction throws", async () => {
    const f = await fixture([row("wf-flip-failed")]);
    f.deps.status = {
      ...f.status,
      async adoptPendingRecording() {
        throw new Error("flip transaction failed");
      },
    };

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]).toMatchObject({
      action: "error",
      reason: "flip transaction failed",
    });
    expect(await f.deps.ledger.get("wf-flip-failed")).toBeNull();
    expect(f.status.inspect("wf-flip-failed")?.status).toBe("PENDING");
  });

  test("three failed adoption transactions do not burn the cap or cancel untouched work", async () => {
    const f = await fixture([row("wf-never-flipped")]);
    f.deps.status = {
      ...f.status,
      async adoptPendingRecording() {
        throw new Error("flip transaction failed");
      },
    };

    for (let attempt = 0; attempt < 4; attempt++) {
      const result = await runSweepTick(f.deps);
      expect(result.decisions[0]).toMatchObject({
        action: "error",
        reason: "flip transaction failed",
      });
    }

    expect(await f.deps.ledger.get("wf-never-flipped")).toBeNull();
    expect(f.cancelled).toEqual([]);
    expect(f.status.inspect("wf-never-flipped")?.status).toBe("PENDING");
  });

  test("records stale-cancel intent before a throwing cancellation", async () => {
    const f = await fixture([
      row("wf-cancel-intent", {
        createdAtEpochMs: NOW.getTime() - 2 * HOUR_MS,
      }),
    ]);
    f.deps.cancelWorkflow = async () => {
      throw new Error("cancel failed");
    };

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]).toMatchObject({
      action: "error",
      reason: "cancel failed",
    });
    expect(await f.deps.ledger.get("wf-cancel-intent")).toMatchObject({
      sweepCount: 1,
      workflowName: "ToolExecWorkflow",
    });
    expect(f.status.inspect("wf-cancel-intent")?.status).toBe("PENDING");
  });

  test("prunes stale heartbeat rows under the lease with a grace-safe floor", async () => {
    const f = await fixture([], {}, { graceMs: 600_000 });
    const pruneCalls: number[] = [];
    f.deps.heartbeats = {
      ...f.deps.heartbeats,
      prune: async (retentionMs) => {
        pruneCalls.push(retentionMs);
        return 2;
      },
    };

    await runSweepTick(f.deps);

    // 7 days dominates the default grace; an absurd grace still wins 2×.
    expect(pruneCalls).toEqual([7 * 24 * HOUR_MS]);

    const wide = await fixture([], {}, { graceMs: 8 * 24 * HOUR_MS });
    const wideCalls: number[] = [];
    wide.deps.heartbeats = {
      ...wide.deps.heartbeats,
      prune: async (retentionMs) => {
        wideCalls.push(retentionMs);
        return 0;
      },
    };
    // The sweeper's own version must stay live under the huge grace window.
    await wide.heartbeats.beat(CURRENT_VERSION, "pod-current");

    await runSweepTick(wide.deps);

    expect(wideCalls).toEqual([16 * 24 * HOUR_MS]);
  });

  test("a failing heartbeat prune does not abort the sweep", async () => {
    const f = await fixture([row("wf-pending")]);
    f.deps.heartbeats = {
      ...f.deps.heartbeats,
      prune: async () => {
        throw new Error("prune unavailable");
      },
    };

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("adopted");
  });

  test("releases the lease even when the tick body throws", async () => {
    const f = await fixture();
    f.deps.status = {
      async listNonTerminalOnVersionsNotIn(): Promise<DbosWorkflowRow[]> {
        throw new Error("status unavailable");
      },
      async adoptPendingRecording() {
        return { flipped: false };
      },
      async clearVersionOnEnqueuedRecording() {
        return { flipped: false };
      },
      async listUnhandledTerminalFailures() {
        return [];
      },
    };

    await expect(runSweepTick(f.deps)).rejects.toThrow("status unavailable");
    expect(await f.lease.tryAcquire("next-owner", 120_000)).toBe(true);
  });

  test("runs both alert passes under the lease and reports their results", async () => {
    const f = await fixture([row("wf-alerted")]);
    const calls: string[] = [];
    f.deps.alerter = {
      async alertDecisions(decisions) {
        expect(decisions).toHaveLength(1);
        calls.push("decisions");
      },
      async scanTerminalFailures() {
        calls.push("failures");
        return {
          scanned: 2,
          alerted: 1,
          cleanupsRun: 1,
          cleanupsFailed: 0,
        };
      },
    };

    const result = await runSweepTick(f.deps);

    expect(calls).toEqual(["decisions", "failures"]);
    expect(result.alerted).toBe(0);
    expect(result.failureScan).toEqual({
      scanned: 2,
      alerted: 1,
      cleanupsRun: 1,
      cleanupsFailed: 0,
    });
    // The alerter ran before the finally block released the lease.
    expect(await f.lease.tryAcquire("next-owner", 120_000)).toBe(true);
  });

  test("contains failures from both alerter passes", async () => {
    const f = await fixture([row("wf-error")]);
    const calls: string[] = [];
    f.deps.alerter = {
      async alertDecisions() {
        calls.push("decisions");
        throw new Error("decision alert failed");
      },
      async scanTerminalFailures() {
        calls.push("failures");
        throw new Error("failure scan failed");
      },
    };

    await expect(runSweepTick(f.deps)).resolves.toMatchObject({
      leaseHeld: true,
      scanned: 1,
    });
    expect(calls).toEqual(["decisions", "failures"]);
    expect(await f.lease.tryAcquire("next-owner", 120_000)).toBe(true);
  });
});

describe("sweep policy exhaustiveness", () => {
  test("accepts the four registered production workflow names", () => {
    expect(() =>
      assertSweepPoliciesExhaustive([
        "SlackThreadWorkflow",
        "PrReviewWorkflow",
        "ToolExecWorkflow",
        "AutomationRunWorkflow",
      ]),
    ).not.toThrow();
  });

  test("lists every unclassified registered workflow", () => {
    expect(() =>
      assertSweepPoliciesExhaustive([
        "SlackThreadWorkflow",
        "FifthWorkflow",
        "SixthWorkflow",
        "_dbos_internal",
        "temp_workflow-send-123",
      ]),
    ).toThrow(/FifthWorkflow.*SixthWorkflow/);
  });
});

describe("VersionHeartbeat", () => {
  test("start performs the first beat before it resolves", async () => {
    const beats: Array<{ appVersion: string; podName: string }> = [];
    let intervalScheduled = false;
    let timerHandle!: ReturnType<typeof setInterval>;
    const heartbeat = new VersionHeartbeat({
      appVersion: () => CURRENT_VERSION,
      podName: "pod-a",
      log,
      heartbeats: {
        async beat(appVersion, podName) {
          beats.push({ appVersion, podName });
        },
        async liveVersions() {
          return [];
        },
        async prune() {
          return 0;
        },
      },
      intervalMs: 30_000,
      setInterval() {
        intervalScheduled = true;
        return timerHandle;
      },
      clearInterval() {},
    });

    await heartbeat.start();

    expect(beats).toEqual([
      { appVersion: CURRENT_VERSION, podName: "pod-a" },
    ]);
    expect(intervalScheduled).toBe(true);
    await heartbeat.stop();
  });
});
