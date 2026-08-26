import { describe, expect, test } from "bun:test";

import type {
  AutomationCronStore,
  AutomationMetaRow,
  AutomationRunRow,
  DueCronAutomation,
} from "../../db/automations.ts";
import type { AutomationDefinition } from "../engine/definition.ts";
import type { AutomationInbox } from "../engine/inbox.ts";
import {
  AUTOMATION_LEASE_TTL_MS,
  automationCronWorkflowId,
  runSchedulerTick,
  type AutomationWorkflowStarter,
} from "../scheduler.ts";

const NOW = new Date("2026-07-22T12:00:00Z");

function meta(scheduledFor = NOW): AutomationMetaRow {
  return {
    id: "automation-1",
    name: "Every minute",
    description: "",
    enabled: true,
    kind: "user",
    builtinKey: null,
    currentVersion: 3,
    inputs: {},
    blockOverrides: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin-1",
    nextFireAt: scheduledFor,
    lastFiredAt: null,
    draftSessionId: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  };
}

function dueAutomation(
  scheduledFor = NOW,
  settings: AutomationDefinition["settings"] = { endSessionsOnFinish: false },
): DueCronAutomation {
  const trigger = { kind: "cron", schedule: "* * * * *", timezone: "UTC" } as const;
  return {
    automation: meta(scheduledFor),
    entrypointId: "main",
    trigger,
    definition: { engine: 1, trigger, blocks: [], inputsSchema: [], settings },
    nextFireAt: scheduledFor,
  };
}

function pendingRun(
  scheduledFor: Date,
  overrides: Partial<AutomationRunRow> = {},
): AutomationRunRow {
  return {
    id: automationCronWorkflowId("automation-1", scheduledFor),
    automationId: "automation-1",
    version: 3,
    entrypointId: "main",
    instanceId: "",
    trigger: { source: "cron", receivedAt: scheduledFor.toISOString() },
    deliveryKey: `cron:${Math.floor(scheduledFor.getTime() / 1_000)}`,
    dryRun: false,
    concurrencyKey: null,
    renderedPrompt: null,
    renderedTitle: null,
    taskId: null,
    sessionId: null,
    status: "pending",
    error: null,
    scheduledFor,
    leaseOwner: "dead-pod",
    leaseExpiresAt: new Date(NOW.getTime() - 1),
    startedAt: null,
    endedAt: null,
    createdAt: scheduledFor,
    ...overrides,
  };
}

function fixture(input?: {
  scheduledFor?: Date;
  existingRun?: AutomationRunRow;
  settings?: AutomationDefinition["settings"];
  /** Pre-existing holder of the concurrency key (simulates an active run). */
  claimHolder?: string;
}) {
  let automation = dueAutomation(input?.scheduledFor, input?.settings);
  let run = input?.existingRun;
  const advances: Array<{ fired: boolean; scheduledFor: Date }> = [];
  const claimVersions: number[] = [];

  const claims = new Map<string, string>();
  const settled: Array<{ runId: string; key: string; terminal?: { status: string; error: string } }> = [];
  const store: AutomationCronStore = {
    async settleRunConcurrency(runId, concurrencyKey, terminal) {
      settled.push({ runId, key: concurrencyKey, ...(terminal ? { terminal } : {}) });
      if (run && run.id === runId) {
        run = {
          ...run,
          concurrencyKey,
          ...(terminal ? { status: terminal.status, error: terminal.error } : {}),
        };
      }
    },
    async claimConcurrency(automationId, key, runId) {
      const mapKey = `${automationId}:${key}`;
      const holder = input?.claimHolder ?? claims.get(mapKey);
      if (holder === undefined || holder === runId) {
        claims.set(mapKey, runId);
        return { claimed: true };
      }
      return { claimed: false, holderRunId: holder };
    },
    async getConcurrencyHolder(automationId, key) {
      return claims.get(`${automationId}:${key}`) ?? null;
    },
    async casConcurrency(automationId, key, fromRunId, toRunId) {
      const mapKey = `${automationId}:${key}`;
      const holder = input?.claimHolder ?? claims.get(mapKey);
      if (holder !== fromRunId) return false;
      claims.set(mapKey, toRunId);
      return true;
    },
    async listDueCron(now) {
      return automation.automation.enabled && automation.nextFireAt <= now ? [automation] : [];
    },
    async claimCronOccurrence(claim) {
      claimVersions.push(claim.version);
      if (!run) {
        run = pendingRun(claim.scheduledFor, {
          id: claim.runId,
          leaseOwner: claim.leaseOwner,
          leaseExpiresAt: claim.leaseExpiresAt,
        });
        return { kind: "claimed", run };
      }
      if (run.status !== "pending") return { kind: "terminal", run };
      if (run.leaseExpiresAt !== null && run.leaseExpiresAt > claim.now) return null;
      run = {
        ...run,
        leaseOwner: claim.leaseOwner,
        leaseExpiresAt: claim.leaseExpiresAt,
      };
      return { kind: "claimed", run };
    },
    async markRunSkipped(runId, reason) {
      if (run?.id === runId && run.status === "pending") {
        run = {
          ...run,
          status: "filtered",
          error: reason,
          leaseOwner: null,
          leaseExpiresAt: null,
        };
      }
    },
    async advanceCronSchedule(advance) {
      if (automation.nextFireAt.getTime() !== advance.scheduledFor.getTime()) return false;
      advances.push({ fired: advance.fired, scheduledFor: advance.scheduledFor });
      automation = {
        ...automation,
        automation: {
          ...automation.automation,
          nextFireAt: advance.nextFireAt,
          ...(advance.fired ? { lastFiredAt: advance.scheduledFor } : {}),
        },
        nextFireAt: advance.nextFireAt,
      };
      return true;
    },
  };

  return {
    store,
    advances,
    claimVersions,
    settled,
    get run() {
      return run;
    },
  };
}

function recordingSender() {
  const sent: Array<{ runId: string; message: AutomationInbox; key: string }> = [];
  return {
    sent,
    async send(runId: string, message: AutomationInbox, key: string) {
      sent.push({ runId, message, key });
    },
  };
}

function recordingStarter(): AutomationWorkflowStarter & {
  starts: Array<{ workflowId: string; runId: string; automationId: string }>;
} {
  const starts: Array<{ workflowId: string; runId: string; automationId: string }> = [];
  return {
    starts,
    async start(input, workflowId) {
      starts.push({ workflowId, runId: input.runId, automationId: input.automationId });
    },
  };
}

describe("automation cron scheduler", () => {
  test("two concurrent ticks claim one row and durably start once", async () => {
    const f = fixture();
    const starter = recordingStarter();
    const deps = {
      store: f.store,
      workflowStarter: starter,
      now: () => NOW,
    };

    await Promise.all([
      runSchedulerTick({ ...deps, owner: "pod-a" }),
      runSchedulerTick({ ...deps, owner: "pod-b" }),
    ]);

    expect(f.run?.id).toBe("autorun:automation-1:cron:1784721600");
    expect(starter.starts).toEqual([
      {
        workflowId: "autorun:automation-1:cron:1784721600",
        runId: "autorun:automation-1:cron:1784721600",
        automationId: "automation-1",
      },
    ]);
    expect(f.advances).toHaveLength(1);
    // The claim pins the automation's current version onto the run.
    expect(f.claimVersions).toEqual([3, 3]);
  });

  test("an expired claim is reacquired and restarts the same workflow id", async () => {
    const scheduledFor = new Date(NOW.getTime() - AUTOMATION_LEASE_TTL_MS);
    const existingRun = pendingRun(scheduledFor);
    const f = fixture({ scheduledFor, existingRun });
    const starter = recordingStarter();

    await runSchedulerTick({
      owner: "replacement-pod",
      store: f.store,
      workflowStarter: starter,
      now: () => NOW,
    });

    expect(f.run?.id).toBe(existingRun.id);
    expect(f.run?.leaseOwner).toBe("replacement-pod");
    expect(starter.starts.map((s) => s.workflowId)).toEqual([
      automationCronWorkflowId(existingRun.automationId, existingRun.scheduledFor!),
    ]);
    // The run id IS the workflow id (ADR 0119 D3).
    expect(starter.starts[0]!.runId).toBe(starter.starts[0]!.workflowId);
  });

  test("an occurrence beyond grace is skipped without starting a workflow", async () => {
    const scheduledFor = new Date(NOW.getTime() - 11 * 60_000);
    const f = fixture({ scheduledFor });
    const starter = recordingStarter();

    const result = await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      workflowStarter: starter,
      now: () => NOW,
    });

    expect(result.skipped).toBe(1);
    expect(starter.starts).toEqual([]);
    expect(f.run?.status).toBe("filtered");
    expect(f.advances).toEqual([{ fired: false, scheduledFor }]);
  });

  test("a terminal filtered occurrence advances without restarting", async () => {
    const scheduledFor = NOW;
    const existingRun = pendingRun(scheduledFor, {
      status: "filtered",
      leaseOwner: null,
      leaseExpiresAt: null,
    });
    const f = fixture({ scheduledFor, existingRun });
    const starter = recordingStarter();

    await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      workflowStarter: starter,
      now: () => NOW,
    });

    expect(starter.starts).toEqual([]);
    expect(f.advances).toEqual([{ fired: false, scheduledFor }]);
  });

  test("workflow id format is pinned to automation and fire epoch seconds", () => {
    expect(automationCronWorkflowId("abc-123", new Date("2026-07-22T12:34:56.999Z")))
      .toBe("autorun:abc-123:cron:1784723696");
  });

  test("cron admission: a configured concurrency key is claimed and stamped on the run", async () => {
    const f = fixture({
      settings: {
        endSessionsOnFinish: false,
        concurrency: { keyTemplate: "nightly", policy: "supersede" },
      },
    });
    const starter = recordingStarter();
    const sender = recordingSender();
    const result = await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      workflowStarter: starter,
      sender,
      now: () => NOW,
    });
    expect(result.started).toBe(1);
    expect(f.settled).toEqual([{ runId: automationCronWorkflowId("automation-1", NOW), key: "nightly" }]);
    expect(f.run?.concurrencyKey).toBe("nightly");
    expect(sender.sent).toEqual([]);
  });

  test("cron admission: supersede signals the holder and still starts; skip settles as filtered and advances", async () => {
    const holder = "autorun:automation-1:cron:1";
    const supersede = fixture({
      settings: { endSessionsOnFinish: false, concurrency: { keyTemplate: "nightly", policy: "supersede" } },
      claimHolder: holder,
    });
    const starter = recordingStarter();
    const sender = recordingSender();
    const r1 = await runSchedulerTick({ owner: "pod-a", store: supersede.store, workflowStarter: starter, sender, now: () => NOW });
    expect(r1.started).toBe(1);
    expect(sender.sent).toEqual([
      {
        runId: holder,
        message: { kind: "supersede", byRunId: automationCronWorkflowId("automation-1", NOW) },
        key: `autorun:${holder}:supersede:${automationCronWorkflowId("automation-1", NOW)}`,
      },
    ]);
    expect(supersede.advances).toEqual([{ fired: true, scheduledFor: NOW }]);

    const skip = fixture({
      settings: { endSessionsOnFinish: false, concurrency: { keyTemplate: "nightly", policy: "skip" } },
      claimHolder: holder,
    });
    const starter2 = recordingStarter();
    const r2 = await runSchedulerTick({ owner: "pod-a", store: skip.store, workflowStarter: starter2, sender: recordingSender(), now: () => NOW });
    expect(r2.started).toBe(0);
    expect(r2.admitted.skipped).toBe(1);
    expect(starter2.starts).toEqual([]);
    expect(skip.run?.status).toBe("filtered");
    // The occurrence happened and was decided: the schedule still advances.
    expect(skip.advances).toEqual([{ fired: true, scheduledFor: NOW }]);
  });
});

describe("cron fan-out over open workstreams (ADR 0120)", () => {
  function fanoutFixture(input: {
    open: string[];
    settings?: AutomationDefinition["settings"];
    /** Rows already claimed with a LIVE lease held by another pod. */
    leaseHeld?: string[];
    /** Rows whose workflow already started and is still RUNNING. */
    running?: string[];
  }) {
    const settings = input.settings ?? {
      endSessionsOnFinish: false,
      instance: { keyTemplate: "k" },
    };
    let automation = dueAutomation(NOW, settings);
    const runs = new Map<string, AutomationRunRow>();
    for (const instanceId of input.leaseHeld ?? []) {
      const id = automationCronWorkflowId("automation-1", NOW, "main", instanceId);
      runs.set(id, pendingRun(NOW, {
        id,
        instanceId,
        leaseOwner: "other-pod",
        leaseExpiresAt: new Date(NOW.getTime() + 60_000),
      }));
    }
    for (const instanceId of input.running ?? []) {
      const id = automationCronWorkflowId("automation-1", NOW, "main", instanceId);
      runs.set(id, pendingRun(NOW, {
        id,
        instanceId,
        status: "running",
        leaseOwner: null,
        leaseExpiresAt: null,
      }));
    }
    const advances: Array<{ fired: boolean }> = [];
    const store: AutomationCronStore = {
      async listDueCron(now) {
        return automation.nextFireAt <= now ? [automation] : [];
      },
      async claimCronOccurrence(claim) {
        const existing = runs.get(claim.runId);
        if (existing) {
          if (existing.status === "running") return { kind: "in_flight", run: existing };
          if (existing.status !== "pending") return { kind: "terminal", run: existing };
          if (existing.leaseExpiresAt !== null && existing.leaseExpiresAt > claim.now) return null;
          return { kind: "claimed", run: existing };
        }
        const run = pendingRun(claim.scheduledFor, {
          id: claim.runId,
          instanceId: claim.instanceId ?? "",
          leaseOwner: claim.leaseOwner,
          leaseExpiresAt: claim.leaseExpiresAt,
        });
        runs.set(claim.runId, run);
        return { kind: "claimed", run };
      },
      async markRunSkipped() {},
      async settleRunConcurrency() {},
      async claimConcurrency() {
        return { claimed: true };
      },
      async getConcurrencyHolder() {
        return null;
      },
      async casConcurrency() {
        return true;
      },
      async advanceCronSchedule(advance) {
        if (automation.nextFireAt.getTime() !== advance.scheduledFor.getTime()) return false;
        advances.push({ fired: advance.fired });
        automation = { ...automation, nextFireAt: advance.nextFireAt };
        return true;
      },
    };
    const instances = {
      async listOpenInstances() {
        return input.open.map((id) => ({
          id,
          automationId: "automation-1",
          key: `key-${id}`,
          status: "open" as const,
          inputs: {},
          openedBy: "",
          openedAt: NOW,
          closedAt: null,
          closeReason: null,
        }));
      },
    };
    return { store, instances, advances, runs };
  }

  test("one tick claims one occurrence per open workstream and advances ONCE", async () => {
    const f = fanoutFixture({ open: ["ai_one", "ai_two", "ai_three"] });
    const starter = recordingStarter();
    const result = await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      instances: f.instances,
      workflowStarter: starter,
      now: () => NOW,
    });
    expect(result).toMatchObject({ due: 1, claimed: 3, started: 3, errors: 0 });
    expect(starter.starts.map((s) => s.workflowId)).toEqual([
      automationCronWorkflowId("automation-1", NOW, "main", "ai_one"),
      automationCronWorkflowId("automation-1", NOW, "main", "ai_two"),
      automationCronWorkflowId("automation-1", NOW, "main", "ai_three"),
    ]);
    expect([...f.runs.values()].map((r) => r.instanceId).sort()).toEqual([
      "ai_one",
      "ai_three",
      "ai_two",
    ]);
    expect(f.advances).toEqual([{ fired: true }]);
  });

  test("zero open workstreams is a quiet tick: no rows, schedule advances unfired", async () => {
    const f = fanoutFixture({ open: [] });
    const starter = recordingStarter();
    const result = await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      instances: f.instances,
      workflowStarter: starter,
      now: () => NOW,
    });
    expect(result).toMatchObject({ claimed: 0, started: 0 });
    expect(f.runs.size).toBe(0);
    expect(f.advances).toEqual([{ fired: false }]);
  });

  test("rows lease-held by another pod are left alone; the holder advances", async () => {
    const f = fanoutFixture({ open: ["ai_one", "ai_two"], leaseHeld: ["ai_one", "ai_two"] });
    const starter = recordingStarter();
    const result = await runSchedulerTick({
      owner: "pod-b",
      store: f.store,
      instances: f.instances,
      workflowStarter: starter,
      now: () => NOW,
    });
    expect(result).toMatchObject({ claimed: 0, started: 0 });
    expect(f.advances).toEqual([]);

    // Mixed: one sibling held, one free — this pod runs the free one but
    // DEFERS the advance: the held row's claimer might die before starting
    // it, and advancing now would strand that occurrence forever (the
    // advance CAS has no instance dimension). Review finding on this PR.
    const g = fanoutFixture({ open: ["ai_one", "ai_two"], leaseHeld: ["ai_one"] });
    const result2 = await runSchedulerTick({
      owner: "pod-b",
      store: g.store,
      instances: g.instances,
      workflowStarter: recordingStarter(),
      now: () => NOW,
    });
    expect(result2).toMatchObject({ claimed: 1, started: 1 });
    expect(g.advances).toEqual([]);

    // After the held lease expires, the next tick reacquires the sibling
    // (the free row is already terminal-started, an idempotent restart) and
    // only THEN advances — no occurrence is ever stranded.
    const later = new Date(NOW.getTime() + 120_001);
    const result3 = await runSchedulerTick({
      owner: "pod-b",
      store: g.store,
      instances: g.instances,
      workflowStarter: recordingStarter(),
      now: () => later,
    });
    expect(result3).toMatchObject({ errors: 0 });
    expect(g.advances).toEqual([{ fired: true }]);
  });

  test("a RUNNING sibling never defers the advance (it already fired)", async () => {
    // Regression for the cadence-freeze finding: a long-running sibling's
    // row is in_flight, not held — the schedule must advance past it.
    const f = fanoutFixture({ open: ["ai_one", "ai_two"], running: ["ai_one"] });
    const starter = recordingStarter();
    const result = await runSchedulerTick({
      owner: "pod-b",
      store: f.store,
      instances: f.instances,
      workflowStarter: starter,
      now: () => NOW,
    });
    // ai_two claims + starts; ai_one is in flight and neither restarted nor
    // waited for.
    expect(result).toMatchObject({ claimed: 1, started: 1, errors: 0 });
    expect(starter.starts.map((s) => s.workflowId)).toEqual([
      automationCronWorkflowId("automation-1", NOW, "main", "ai_two"),
    ]);
    expect(f.advances).toEqual([{ fired: true }]);
  });

  test("a non-instanced automation never touches the instance store", async () => {
    const f = fanoutFixture({
      open: [],
      settings: { endSessionsOnFinish: false },
    });
    const starter = recordingStarter();
    const result = await runSchedulerTick({
      owner: "pod-a",
      store: f.store,
      instances: {
        async listOpenInstances(): Promise<never[]> {
          throw new Error("must not be called for a non-instanced automation");
        },
      },
      workflowStarter: starter,
      now: () => NOW,
    });
    expect(result).toMatchObject({ claimed: 1, started: 1 });
    expect(f.advances).toEqual([{ fired: true }]);
  });
});
