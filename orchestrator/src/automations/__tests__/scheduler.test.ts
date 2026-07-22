import { describe, expect, test } from "bun:test";

import type {
  AutomationCronStore,
  AutomationRunRow,
  DueCronAutomation,
} from "../../db/automations.ts";
import {
  AUTOMATION_LEASE_TTL_MS,
  automationCronWorkflowId,
  runSchedulerTick,
  type AutomationWorkflowStarter,
} from "../scheduler.ts";

const NOW = new Date("2026-07-22T12:00:00Z");

function dueAutomation(scheduledFor = NOW): DueCronAutomation {
  return {
    id: "automation-1",
    name: "Every minute",
    description: "",
    enabled: true,
    trigger: { kind: "cron", schedule: "* * * * *", timezone: "UTC" },
    action: {
      kind: "create_task",
      profileId: "profile-1",
      promptTemplate: "Run",
      includeEventContext: false,
    },
    createdByUserId: "admin-1",
    nextFireAt: scheduledFor,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  };
}

function pendingRun(
  scheduledFor: Date,
  overrides: Partial<AutomationRunRow> = {},
): AutomationRunRow {
  return {
    id: "run-1",
    automationId: "automation-1",
    trigger: { source: "cron" },
    renderedPrompt: null,
    renderedTitle: null,
    taskId: null,
    sessionId: null,
    status: "pending",
    error: null,
    scheduledFor,
    leaseOwner: "dead-pod",
    leaseExpiresAt: new Date(NOW.getTime() - 1),
    createdAt: scheduledFor,
    ...overrides,
  };
}

function fixture(input?: {
  scheduledFor?: Date;
  existingRun?: AutomationRunRow;
}) {
  let automation = dueAutomation(input?.scheduledFor);
  let run = input?.existingRun;
  let sequence = 0;
  const advances: Array<{ fired: boolean; scheduledFor: Date }> = [];

  const store: AutomationCronStore = {
    async listDueCron(now) {
      return automation.enabled && automation.nextFireAt <= now ? [automation] : [];
    },
    async claimCronOccurrence(claim) {
      if (!run) {
        run = pendingRun(claim.scheduledFor, {
          id: `run-${++sequence}`,
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
          status: "skipped",
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
        nextFireAt: advance.nextFireAt,
        ...(advance.fired ? { lastFiredAt: advance.scheduledFor } : {}),
      };
      return true;
    },
  };

  return {
    store,
    advances,
    get run() {
      return run;
    },
  };
}

function recordingStarter(): AutomationWorkflowStarter & { starts: string[] } {
  const starts: string[] = [];
  return {
    starts,
    async start(_input, workflowId) {
      starts.push(workflowId);
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

    expect(f.run?.id).toBe("run-1");
    expect(starter.starts).toEqual(["auto:automation-1:1784721600"]);
    expect(f.advances).toHaveLength(1);
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
    expect(starter.starts).toEqual([
      automationCronWorkflowId(existingRun.automationId, existingRun.scheduledFor!),
    ]);
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
    expect(f.run?.status).toBe("skipped");
    expect(f.advances).toEqual([{ fired: false, scheduledFor }]);
  });

  test("workflow id format is pinned to automation and fire epoch seconds", () => {
    expect(automationCronWorkflowId("abc-123", new Date("2026-07-22T12:34:56.999Z")))
      .toBe("auto:abc-123:1784723696");
  });
});
