import { afterEach, describe, expect, test } from "bun:test";
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
import {
  assertSweepPoliciesExhaustive,
  SWEEP_POLICIES,
} from "../policy.ts";
import {
  DEFAULT_SWEEP_CONFIG,
  runSweepTick,
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
  const status = makeInMemoryDbosStatusStore(rows, now);
  const cancelled: string[] = [];
  const deps: SweepTickDeps = {
    owner: "sweeper-owner",
    appVersion: () => CURRENT_VERSION,
    config: { ...DEFAULT_SWEEP_CONFIG, ...config },
    heartbeats,
    lease,
    ledger: makeInMemorySweepLedgerStore(now),
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

afterEach(() => {
  delete SWEEP_POLICIES.CancelTestWorkflow;
  delete SWEEP_POLICIES.IgnoreTestWorkflow;
});

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

  test("adopts a fresh PENDING workflow and records the sweep first", async () => {
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

  test("cancels a workflow whose policy mode is cancel", async () => {
    SWEEP_POLICIES.CancelTestWorkflow = {
      mode: "cancel",
      staleAfterHours: 48,
    };
    const f = await fixture([
      row("wf-policy-cancel", { name: "CancelTestWorkflow" }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("cancelled_policy");
    expect(f.cancelled).toEqual(["wf-policy-cancel"]);
    expect((await f.deps.ledger.get("wf-policy-cancel"))?.sweepCount).toBe(1);
  });

  test("ignores a workflow whose policy mode is ignore", async () => {
    SWEEP_POLICIES.IgnoreTestWorkflow = {
      mode: "ignore",
      staleAfterHours: 48,
    };
    const f = await fixture([
      row("wf-policy-ignore", { name: "IgnoreTestWorkflow" }),
    ]);

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]?.action).toBe("ignored");
    expect(f.status.inspect("wf-policy-ignore")?.status).toBe("PENDING");
  });

  test("caps mutating actions while scanned reflects the full fetched list", async () => {
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

    expect(result.scanned).toBe(3);
    expect(result.decisions.map((decision) => decision.action)).toEqual([
      "alert_only",
      "adopted",
    ]);
    expect(f.status.inspect("wf-1")?.applicationVersion).toBeNull();
    expect(f.status.inspect("wf-2")?.applicationVersion).toBe(DEAD_VERSION);
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
      async adoptPending() {
        return [];
      },
    };
    const f = await fixture([], { status });

    const result = await runSweepTick(f.deps);

    expect(result.decisions[0]).toMatchObject({
      action: "error",
      reason: "no longer PENDING",
    });
  });

  test("releases the lease even when the tick body throws", async () => {
    const f = await fixture();
    f.deps.status = {
      async listNonTerminalOnVersionsNotIn(): Promise<DbosWorkflowRow[]> {
        throw new Error("status unavailable");
      },
      async adoptPending() {
        return [];
      },
      async clearVersionOnEnqueued() {
        return [];
      },
      async listNewlyTerminalFailed() {
        return [];
      },
    };

    await expect(runSweepTick(f.deps)).rejects.toThrow("status unavailable");
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
