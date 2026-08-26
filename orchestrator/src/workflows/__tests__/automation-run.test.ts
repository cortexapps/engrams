import { describe, expect, test } from "bun:test";

import type {
  AutomationEngineStore,
} from "../../db/automations.ts";
import type { RunSnapshot } from "../../automations/engine/context.ts";
import type { EngineDeps } from "../../automations/engine/deps.ts";
import type { CreateSessionForExistingTaskParams } from "../../rpc/task-create.ts";
import {
  automationRunWorkflowImpl,
  makeProductionSessionOps,
} from "../automation-run.ts";

const RUN_ID = "autorun:auto-1:manual:x";

function fakeEngineStore(overrides: Partial<AutomationEngineStore> = {}): AutomationEngineStore {
  const snapshot: RunSnapshot = {
    definition: {
      engine: 1,
      trigger: { kind: "manual" },
      blocks: [],
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    },
    inputs: {},
    automationId: "auto-1",
    automationName: "Test",
    version: 1,
    trigger: { kind: "manual", receivedAt: "2026-08-21T00:00:00Z" },
    aliases: [],
    startedAtMs: 1_000,
  };
  return {
    async loadSnapshot() {
      return snapshot;
    },
    async listRunningInstanceRunIds() {
      return [];
    },
    async markRunning() {},
    async recordStep() {},
    async finalizeRun() {},
    async adoptSession() {
      return "foreign" as const;
    },
    async getSessionBinding() {
      return null;
    },
    async listRunSessions() {
      return [];
    },
    async releaseConcurrency() {
      return null;
    },
    async getRun() {
      return null;
    },
    async ensureAutomationTask() {
      return "automation:run";
    },
    async getAutomationTaskSession() {
      return null;
    },
    async recordSessionBinding() {},
    async recordRunLaunch() {},
    async setSessionRelay() {},
    async findSessionBinding() {
      return null;
    },
    ...overrides,
  };
}

describe("automationRunWorkflowImpl", () => {
  test("the registered body is a thin snapshot + interpret call", async () => {
    const names: string[] = [];
    const finalized: string[] = [];
    const store = fakeEngineStore({
      async finalizeRun(_runId, status) {
        finalized.push(status);
      },
    });
    const engine: EngineDeps = {
      step: async (fn, name) => {
        names.push(name);
        return fn();
      },
      recv: async () => null,
      store,
      sessions: {
        createSession: async () => ({ sessionId: "s", taskId: "t" }),
        setSessionRelay: async () => {},
        sendPrompt: async () => {},
        endSession: async () => {},
        exec: async () => ({ exitStatus: 0, stdout: "", stderr: "" }),
        writeFiles: async () => [],
        getSession: async () => ({ found: false }),
      },
      clock: { nowMs: () => 1_000 },
    };

    const result = await automationRunWorkflowImpl(
      { runId: RUN_ID, automationId: "auto-1" },
      { engine },
    );

    expect(result.status).toBe("completed");
    expect(names).toEqual(["step:__snapshot__:0", "step:__finalize__:0"]);
    expect(finalized).toEqual(["completed"]);
  });
});

describe("makeProductionSessionOps.createSession", () => {
  function harness() {
    const calls: string[] = [];
    const createdParams: CreateSessionForExistingTaskParams[] = [];
    const bindings: Array<Record<string, unknown>> = [];
    const launches: Array<Record<string, unknown>> = [];
    let existingPrimary: string | null = null;
    const taskOwners: Array<string | null | undefined> = [];
    const store = fakeEngineStore({
      async ensureAutomationTask(input) {
        calls.push("task");
        taskOwners.push(input.createdByUserId);
        return `automation:${input.runId}`;
      },
      async getAutomationTaskSession() {
        calls.push("existing");
        return existingPrimary;
      },
      async recordSessionBinding(input) {
        calls.push("binding");
        bindings.push({ ...input });
      },
      async recordRunLaunch(input) {
        calls.push("launch");
        launches.push({ ...input });
      },
    });
    const ops = makeProductionSessionOps({
      store,
      createSessionForExistingTask: async (params) => {
        calls.push("create");
        createdParams.push(params);
        return { sessionId: "s-new" };
      },
      registerListener: async () => {
        calls.push("listener");
      },
    });
    return {
      ops,
      calls,
      createdParams,
      bindings,
      launches,
      taskOwners,
      setExistingPrimary(id: string) {
        existingPrimary = id;
      },
    };
  }

  const input = {
    runId: RUN_ID,
    blockId: "launch",
    automationId: "auto-1",
    profileId: "p1",
    prompt: "go",
    title: null,
    role: "primary",
    keep: true,
  };

  test("binding lands BEFORE listener registration; launch denormalizes", async () => {
    const h = harness();
    const created = await h.ops.createSession(input);

    expect(created).toEqual({ sessionId: "s-new", taskId: `automation:${RUN_ID}` });
    // The review-control-plane ordering: create → binding → listener.
    expect(h.calls).toEqual(["task", "existing", "create", "binding", "listener", "launch"]);
    expect(h.bindings[0]).toMatchObject({ sessionId: "s-new", runId: RUN_ID, keep: true });
    expect(h.launches[0]).toMatchObject({ runId: RUN_ID, sessionId: "s-new" });
    const params = h.createdParams[0]!;
    expect(params.taskType).toBe("automation");
    expect(params.integrationPrincipalId).toBe("automation:auto-1");
    expect(params.registerListener).toBe(false);
    expect(params.role).toBe("primary");
  });

  test("an owner (the Slack brain's identity gate) rides through to the session and the task", async () => {
    const h = harness();
    await h.ops.createSession({ ...input, ownerUserId: "user-1" });
    const params = h.createdParams[0]!;
    expect(params.ownerUserId).toBe("user-1");
    expect(h.taskOwners).toEqual(["user-1"]);
    // Without an owner nothing is stamped: the programmatic path is unchanged.
    const h2 = harness();
    await h2.ops.createSession(input);
    expect(h2.createdParams[0]!.ownerUserId).toBeUndefined();
    expect(h2.taskOwners).toEqual([undefined]);
  });

  test("a primary launch reuses the task's existing primary session", async () => {
    const h = harness();
    h.setExistingPrimary("s-existing");
    const created = await h.ops.createSession(input);
    expect(created.sessionId).toBe("s-existing");
    expect(h.calls).toEqual(["task", "existing"]);
  });

  test("non-primary roles always create a fresh session", async () => {
    const h = harness();
    h.setExistingPrimary("s-existing");
    const created = await h.ops.createSession({ ...input, role: "verifier", keep: false });
    expect(created.sessionId).toBe("s-new");
    expect(h.calls).toContain("create");
    // Only the primary launch denormalizes onto the run row.
    expect(h.launches).toEqual([]);
    expect(h.bindings[0]).toMatchObject({ role: "verifier", keep: false });
  });
});
