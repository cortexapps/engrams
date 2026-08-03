import { describe, expect, test } from "bun:test";

import type {
  AutomationRow,
  AutomationRunRow,
  AutomationWorkflowStore,
} from "../../db/automations.ts";
import type { CreateSessionForExistingTaskParams } from "../../rpc/task-create.ts";
import {
  automationRunWorkflowImpl,
  makeAutomationTaskCreator,
  type AutomationRunWorkflowInput,
  type AutomationStepRunner,
  type PreparedAutomationRun,
} from "../automation-run.ts";

const NOW = new Date("2026-07-22T12:00:00Z");
const input: AutomationRunWorkflowInput = {
  automationId: "automation-1",
  runId: "run-1",
  trigger: { source: "cron" },
  scheduledFor: NOW.toISOString(),
  receivedAt: NOW.toISOString(),
};

function automation(
  promptTemplate: string,
  override: { harness?: string; model?: string; effort?: string } = {},
): AutomationRow {
  return {
    id: input.automationId,
    name: "Daily triage",
    description: "",
    enabled: true,
    trigger: { kind: "cron", schedule: "0 12 * * *", timezone: "UTC" },
    action: {
      kind: "create_task",
      profileId: "profile-1",
      promptTemplate,
      includeEventContext: true,
      ...override,
    },
    createdByUserId: "admin-1",
    nextFireAt: NOW,
    lastFiredAt: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
  };
}

function run(status = "pending"): AutomationRunRow {
  return {
    id: input.runId,
    automationId: input.automationId,
    trigger: input.trigger,
    renderedPrompt: null,
    renderedTitle: null,
    taskId: null,
    sessionId: null,
    status,
    error: null,
    scheduledFor: NOW,
    leaseOwner: "pod-a",
    leaseExpiresAt: new Date(NOW.getTime() + 120_000),
    createdAt: NOW,
  };
}

function fixture(
  promptTemplate = "Run at ${{ trigger.scheduled_for }}",
  status = "pending",
  override: { harness?: string; model?: string; effort?: string } = {},
) {
  let runRow = run(status);
  const automationRow = automation(promptTemplate, override);
  const store: AutomationWorkflowStore = {
    async ensureRun() {
      return runRow;
    },
    async getRun() {
      return runRow;
    },
    async getAutomation() {
      return automationRow;
    },
    async recordRendered(_runId, prompt, title) {
      runRow = { ...runRow, renderedPrompt: prompt, renderedTitle: title };
    },
    async ensureAutomationTask() {
      runRow = { ...runRow, taskId: "task-1" };
      return "task-1";
    },
    async getAutomationTaskSession() {
      return null;
    },
    async markRunRenderFailed(_runId, error) {
      runRow = { ...runRow, status: "render_failed", error };
    },
    async markRunLaunchFailed(_runId, error) {
      runRow = { ...runRow, status: "launch_failed", error };
    },
    async markRunLaunched(_runId, taskId, sessionId) {
      runRow = { ...runRow, status: "launched", taskId, sessionId };
    },
  };
  return {
    store,
    get run() {
      return runRow;
    },
  };
}

function immediateSteps(): { step: AutomationStepRunner; names: string[] } {
  const names: string[] = [];
  return {
    names,
    step: async (fn, name) => {
      names.push(name);
      return fn();
    },
  };
}

describe("AutomationRunWorkflow", () => {
  test("a template error stamps render_failed and creates no task or session", async () => {
    const f = fixture("${{ event.issue.title }}");
    const steps = immediateSteps();
    let launches = 0;

    await automationRunWorkflowImpl(input, {
      store: f.store,
      step: steps.step,
      taskCreator: {
        async create() {
          launches++;
          return { taskId: "unexpected", sessionId: "unexpected" };
        },
      },
    });

    expect(f.run.status).toBe("render_failed");
    expect(f.run.error).toContain("undefined variable");
    expect(f.run.taskId).toBeNull();
    expect(f.run.sessionId).toBeNull();
    expect(launches).toBe(0);
    expect(steps.names).toEqual([
      "ensureAutomationRun",
      "renderAutomationAction",
      "markAutomationRenderFailed",
    ]);
  });

  test("a terminal occurrence is a no-op even if invoked again", async () => {
    const f = fixture("Run", "launched");
    const steps = immediateSteps();
    let launches = 0;

    await automationRunWorkflowImpl(input, {
      store: f.store,
      step: steps.step,
      taskCreator: {
        async create() {
          launches++;
          return { taskId: "unexpected", sessionId: "unexpected" };
        },
      },
    });

    expect(launches).toBe(0);
    expect(steps.names).toEqual(["ensureAutomationRun"]);
    expect(f.run.status).toBe("launched");
  });

  test("a successful launch records rendered values and task/session ids", async () => {
    const f = fixture("Run at ${{ trigger.scheduled_for }}");
    const steps = immediateSteps();

    await automationRunWorkflowImpl(input, {
      store: f.store,
      step: steps.step,
      taskCreator: {
        async create(_workflowInput, prepared) {
          expect(prepared).toEqual({
            profileId: "profile-1",
            prompt: `Run at ${NOW.toISOString()}`,
            title: null,
          });
          return { taskId: "task-1", sessionId: "session-1" };
        },
      },
    });

    expect(f.run).toMatchObject({
      status: "launched",
      renderedPrompt: `Run at ${NOW.toISOString()}`,
      taskId: "task-1",
      sessionId: "session-1",
    });
  });

  test("the stored harness/model/effort override reaches the launch step", async () => {
    const f = fixture("Run", "pending", { harness: "codex", model: "gpt", effort: "high" });
    let prepared: PreparedAutomationRun | undefined;

    await automationRunWorkflowImpl(input, {
      store: f.store,
      step: immediateSteps().step,
      taskCreator: {
        async create(_input, value) {
          prepared = value;
          return { taskId: "task-1", sessionId: "session-1" };
        },
      },
    });

    expect(prepared).toEqual({
      profileId: "profile-1",
      prompt: "Run",
      title: null,
      harness: "codex",
      model: "gpt",
      effort: "high",
    });
  });

  test("an automation with no override prepares no harness selection", async () => {
    const f = fixture("Run");
    let prepared: PreparedAutomationRun | undefined;

    await automationRunWorkflowImpl(input, {
      store: f.store,
      step: immediateSteps().step,
      taskCreator: {
        async create(_input, value) {
          prepared = value;
          return { taskId: "task-1", sessionId: "session-1" };
        },
      },
    });

    expect(prepared).not.toHaveProperty("harness");
    expect(prepared).not.toHaveProperty("model");
    expect(prepared).not.toHaveProperty("effort");
  });

  test("a webhook run renders declarative connector aliases from its redacted payload", async () => {
    const webhookInput: AutomationRunWorkflowInput = {
      automationId: "automation-1",
      runId: "webhook-run",
      trigger: {
        source: "webhook",
        eventKey: "issues.opened",
        deliveryId: "delivery-1",
        payload: { issue: { title: "Broken build" } },
      },
      receivedAt: NOW.toISOString(),
    };
    const f = fixture("Triage ${{ event.issue.title }}");
    const webhookAutomation = automation("Triage ${{ event.issue.title }}");
    webhookAutomation.trigger = {
      kind: "webhook",
      registrationId: "github-app",
      events: ["issues.opened"],
    };
    const store: AutomationWorkflowStore = {
      ...f.store,
      async getAutomation() {
        return webhookAutomation;
      },
    };
    let preparedPrompt = "";

    await automationRunWorkflowImpl(webhookInput, {
      store,
      step: immediateSteps().step,
      aliases: async (registrationId) => {
        expect(registrationId).toBe("github-app");
        return [{ path: "issue.title", alias: "issue.title" }];
      },
      taskCreator: {
        async create(_input, prepared) {
          preparedPrompt = prepared.prompt;
          return { taskId: "task-1", sessionId: "session-1" };
        },
      },
    });

    expect(preparedPrompt).toContain("Triage Broken build");
  });
});

describe("automation task creator", () => {
  test("uses a null owner and keeps the selected profile policy intact", async () => {
    const f = fixture("Run");
    let taskInput: Parameters<AutomationWorkflowStore["ensureAutomationTask"]>[0] | undefined;
    let sessionInput: CreateSessionForExistingTaskParams | undefined;
    const store: AutomationWorkflowStore = {
      ...f.store,
      async ensureAutomationTask(value) {
        taskInput = value;
        return "task-1";
      },
    };
    const creator = makeAutomationTaskCreator({
      store,
      async createSessionForExistingTask(value) {
        sessionInput = value;
        return { sessionId: "session-1" };
      },
    });

    const created = await creator.create(input, {
      profileId: "profile-1",
      prompt: "Do the work",
      title: null,
    });

    expect(created).toEqual({ taskId: "task-1", sessionId: "session-1" });
    expect(taskInput).toEqual({
      runId: "run-1",
      automationId: "automation-1",
      title: "Do the work",
      source: {
        provider: "automation",
        automationId: "automation-1",
        runId: "run-1",
        trigger: { kind: "cron", scheduledFor: NOW.toISOString() },
      },
    });
    expect(sessionInput).toEqual({
      taskId: "task-1",
      profileId: "profile-1",
      integrationPrincipalId: "automation:automation-1",
      role: "primary",
      prompt: "Do the work",
      registerListener: true,
    });
    expect(sessionInput).not.toHaveProperty("ownerUserId");
    expect(sessionInput).not.toHaveProperty("dropProfileSecretsAndEnv");
    expect(sessionInput).not.toHaveProperty("capabilityOverride");
    expect(sessionInput).not.toHaveProperty("networkOverride");
    expect(sessionInput).not.toHaveProperty("harness");
    expect(sessionInput).not.toHaveProperty("model");
    expect(sessionInput).not.toHaveProperty("effort");
  });

  test("forwards the prepared harness/model/effort to session create", async () => {
    const f = fixture("Run");
    let sessionInput: CreateSessionForExistingTaskParams | undefined;
    const creator = makeAutomationTaskCreator({
      store: f.store,
      async createSessionForExistingTask(value) {
        sessionInput = value;
        return { sessionId: "session-1" };
      },
    });

    await creator.create(input, {
      profileId: "profile-1",
      prompt: "Do the work",
      title: null,
      harness: "codex",
      model: "gpt",
      effort: "high",
    });

    expect(sessionInput).toMatchObject({ harness: "codex", model: "gpt", effort: "high" });
  });
});
