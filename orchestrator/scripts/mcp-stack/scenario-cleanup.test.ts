import { expect, test } from "bun:test";
import {
  Code,
  ConnectError,
  createClient,
  createRouterTransport,
} from "@connectrpc/connect";
import { closeScenario, closeScenarioSession } from "./scenario-runtime.ts";
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
    failure?: "stop" | "delete" | "get" | "retain" | "destroy" | "settled";
    destroyStates?: Array<
      "queued" | "running" | "done" | "failed" | "cancelled"
    >;
    settled?: boolean[];
  } = {},
) {
  const events: string[] = [];
  const sessionId = "11111111-1111-4111-8111-111111111111";
  const ctx = {
    hostId: "22222222-2222-4222-8222-222222222222",
    deadlines: { requestMs: 1000, stepMs: 0, totalMs: 10000 },
  };
  const statuses = [...(input.statuses ?? ["completed"])];
  const destroyStates = [...(input.destroyStates ?? ["done"])];
  const settled = [...(input.settled ?? [true])];
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
    database: {
      async retainCleanupTarget(id: string, hostId: string) {
        expect(id).toBe(sessionId);
        expect(hostId).toBe(ctx.hostId);
        events.push("retainTarget");
        if (input.failure === "retain") throw new Error("retain failed");
      },
      async destroyOperation(id: string) {
        expect(id).toBe(sessionId);
        const state =
          destroyStates.length > 1 ? destroyStates.shift()! : destroyStates[0]!;
        events.push(`destroy:${state}`);
        if (input.failure === "destroy") throw new Error("destroy failed");
        return { id: "1", state };
      },
      async cleanupSettled(id: string) {
        expect(id).toBe(sessionId);
        const value = settled.length > 1 ? settled.shift()! : settled[0]!;
        events.push(`settled:${value}`);
        if (input.failure === "settled") throw new Error("settled failed");
        return value;
      },
    },
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
  return {
    events,
    ctx,
    close: () => closeScenario(principals, scenario),
    closeSession: () =>
      closeScenarioSession(principals.admin.sessions, scenario),
  };
}

test("automation cleanup waits for the exact primary session before removing ownership", async () => {
  const fixture = cleanupFixture({ statuses: ["active", "completed"] });
  fixture.ctx.deadlines.stepMs = 500;
  await fixture.close();
  expect(fixture.events).toEqual([
    "getRun",
    "stopRun",
    "retainTarget",
    "deleteSession",
    "getSession",
    "getSession",
    "destroy:done",
    "settled:true",
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
      "retainTarget",
      "deleteSession",
      "getSession",
      "destroy:done",
      "settled:true",
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
    "retainTarget",
    "deleteSession",
    "getSession",
    "destroy:done",
    "settled:true",
    "deleteTask",
    "deleteProfile",
  ]);
});

test("cleanup errors retain the task and definition", async () => {
  for (const failure of [
    "stop",
    "delete",
    "get",
    "retain",
    "destroy",
    "settled",
  ] as const) {
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
    "retainTarget",
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
    "retainTarget",
    "deleteSession",
    "getSession",
  ]);
});

for (const state of ["queued", "running", "failed", "cancelled"] as const)
  test(`terminal session with destroy ${state} retains ownership`, async () => {
    const fixture = cleanupFixture({ destroyStates: [state] });
    await expect(fixture.close()).rejects.toThrow();
    expect(fixture.events).toContain(`destroy:${state}`);
    expect(fixture.events).not.toContain("archiveAutomation");
    expect(fixture.events).not.toContain("deleteTask");
    expect(fixture.events).not.toContain("deleteProfile");
  });

test("completed destroy with pending cleanup obligations retains ownership", async () => {
  const fixture = cleanupFixture({ settled: [false] });
  await expect(fixture.close()).rejects.toThrow();
  expect(fixture.events).toContain("settled:false");
  expect(fixture.events).not.toContain("deleteTask");
  expect(fixture.events).not.toContain("deleteProfile");
});

test("child session uses the same retirement barrier before task ownership can be removed", async () => {
  const blocked = cleanupFixture({ automation: false, settled: [false] });
  await expect(blocked.closeSession()).rejects.toThrow(
    "scenario_cleanup_obligations",
  );
  expect(blocked.events).toEqual([
    "retainTarget",
    "deleteSession",
    "getSession",
    "destroy:done",
    "settled:false",
  ]);
  const settled = cleanupFixture({ automation: false });
  await settled.closeSession();
  expect(settled.events).toEqual([
    "retainTarget",
    "deleteSession",
    "getSession",
    "destroy:done",
    "settled:true",
  ]);
});

test("cleanup waits for destroy and obligations before the next case can start", async () => {
  const fixture = cleanupFixture({
    destroyStates: ["running", "done"],
    settled: [false, true],
  });
  fixture.ctx.deadlines.stepMs = 1000;
  await fixture.close();
  fixture.events.push("nextCase");
  expect(fixture.events.slice(-8)).toEqual([
    "destroy:done",
    "settled:false",
    "destroy:done",
    "settled:true",
    "archiveAutomation",
    "deleteTask",
    "deleteProfile",
    "nextCase",
  ]);
  expect(fixture.events.indexOf("retainTarget")).toBeLessThan(
    fixture.events.indexOf("deleteSession"),
  );
});
