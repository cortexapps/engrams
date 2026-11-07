import { expect, test } from "bun:test";
import {
  Code,
  ConnectError,
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";
import { closeScenario } from "./scenario-runtime.ts";
import {
  AutomationService,
  AutomationRunService,
} from "../../src/gen/engram/app/v1/automation_pb.ts";
import { SessionService } from "../../src/gen/engram/app/v1/session_pb.ts";
import { TaskService } from "../../src/gen/engram/app/v1/task_pb.ts";
import { ProfileService } from "../../src/gen/engram/app/v1/profile_pb.ts";

function cleanupFixture(
  input: {
    automation?: boolean;
    runStatus?: string;
    statuses?: string[];
    wrongSession?: boolean;
    failure?: "stop" | "delete" | "get";
  } = {},
) {
  const events: string[] = [];
  const sessionId = "11111111-1111-4111-8111-111111111111";
  const ctx = { deadlines: { requestMs: 1000, stepMs: 0, totalMs: 10000 } };
  const statuses = [...(input.statuses ?? ["completed"])];
  const transport = createRouterTransport((router) => {
    router.service(AutomationRunService, {
      getRun(request) {
        expect(request.runId).toBe("fixture-run");
        events.push("getRun");
        return { run: { brief: { status: input.runStatus ?? "running" } } };
      },
      stopRun(request) {
        expect(request.runId).toBe("fixture-run");
        events.push("stopRun");
        if (input.failure === "stop")
          throw new ConnectError("stop failed", Code.Unavailable);
        return {};
      },
    });
    router.service(SessionService, {
      deleteSession(request) {
        expect(request.sessionId).toBe(sessionId);
        events.push("deleteSession");
        if (input.failure === "delete")
          throw new ConnectError("delete failed", Code.Unavailable);
        return {};
      },
      getSession(request) {
        expect(request.sessionId).toBe(sessionId);
        events.push("getSession");
        if (input.failure === "get")
          throw new ConnectError("get failed", Code.Unavailable);
        return {
          session: {
            id: input.wrongSession ? "other-session" : sessionId,
            status: statuses.length > 1 ? statuses.shift() : statuses[0],
          },
        };
      },
    });
    router.service(AutomationService, {
      archiveAutomation(request) {
        expect(request.id).toBe("fixture-automation");
        events.push("archiveAutomation");
        return {};
      },
    });
    router.service(TaskService, {
      deleteTask(request) {
        expect(request.taskId).toBe("fixture-task");
        events.push("deleteTask");
        return {};
      },
    });
    router.service(ProfileService, {
      deleteProfile(request) {
        expect(request.id).toBe("fixture-profile");
        events.push("deleteProfile");
        return {};
      },
    });
  });
  const principals = {
    automation: {
      automationRuns: createClient(AutomationRunService, transport),
    },
    admin: {
      sessions: createClient(SessionService, transport),
      automations: createClient(AutomationService, transport),
      tasks: createClient(TaskService, transport),
      profiles: createClient(ProfileService, transport),
    },
  };
  const scenario = {
    ctx,
    taskId: "fixture-task",
    sessionId,
    profileId: "fixture-profile",
    automation:
      input.automation === false
        ? undefined
        : {
            id: "fixture-automation",
            runId: "fixture-run",
            runtimeActorId: "fixture-runtime",
            definitionActorId: "fixture-definition",
            triggerActorId: "fixture-trigger",
            definitionSha256: "a".repeat(64),
          },
  };
  return { events, ctx, close: () => closeScenario(principals, scenario) };
}

test("automation cleanup waits for the exact primary session before removing ownership", async () => {
  const fixture = cleanupFixture({ statuses: ["active", "completed"] });
  fixture.ctx.deadlines.stepMs = 500;
  await fixture.close();
  expect(fixture.events).toEqual([
    "getRun",
    "stopRun",
    "deleteSession",
    "getSession",
    "getSession",
    "archiveAutomation",
    "deleteTask",
    "deleteProfile",
  ]);
});

test("terminal automation runs still require primary session cleanup", async () => {
  for (const status of ["completed", "failed", "dead"]) {
    const fixture = cleanupFixture({
      runStatus: "completed",
      statuses: [status],
    });
    await fixture.close();
    expect(fixture.events).toEqual([
      "getRun",
      "deleteSession",
      "getSession",
      "archiveAutomation",
      "deleteTask",
      "deleteProfile",
    ]);
  }
});

test("human cleanup deletes and confirms the exact primary session", async () => {
  const fixture = cleanupFixture({ automation: false });
  await fixture.close();
  expect(fixture.events).toEqual([
    "deleteSession",
    "getSession",
    "deleteTask",
    "deleteProfile",
  ]);
});

test("cleanup errors retain the task and definition", async () => {
  for (const failure of ["stop", "delete", "get"] as const) {
    const fixture = cleanupFixture({ failure });
    await expect(fixture.close()).rejects.toThrow(`${failure} failed`);
    expect(fixture.events).not.toContain("archiveAutomation");
    expect(fixture.events).not.toContain("deleteTask");
    expect(fixture.events).not.toContain("deleteProfile");
    if (failure === "stop")
      expect(fixture.events).not.toContain("deleteSession");
  }
});

test("wrong session identity cannot authorize ownership deletion", async () => {
  const fixture = cleanupFixture({ wrongSession: true });
  await expect(fixture.close()).rejects.toThrow();
  expect(fixture.events).toEqual([
    "getRun",
    "stopRun",
    "deleteSession",
    "getSession",
  ]);
});

test("nonterminal session timeout retains ownership", async () => {
  const fixture = cleanupFixture({ statuses: ["active"] });
  await expect(fixture.close()).rejects.toThrow();
  expect(fixture.events).toEqual([
    "getRun",
    "stopRun",
    "deleteSession",
    "getSession",
  ]);
});
