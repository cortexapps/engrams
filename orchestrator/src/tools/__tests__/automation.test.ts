import { describe, expect, test } from "bun:test";

import { AUTOMATION_TOPIC, type AutomationInbox } from "../../automations/engine/inbox.ts";
import { registerAutomationTools, type AutomationSignalNotify } from "../automation.ts";
import { createToolRegistry, type ToolContext } from "../registry.ts";
import { compileToolManifest } from "../manifest.ts";

const RUN_ID = "autorun:auto-1:webhook:d1";

function context(): ToolContext {
  return {
    sessionId: "session-1",
    taskId: "task-1",
    capabilities: [],
    toolCallId: "call-1",
    toolName: "signal_automation",
  };
}

function harness(
  options: {
    bound?: boolean;
    failNotify?: boolean | ((destinationId: string) => boolean);
    binding?: {
      runId: string;
      automationId?: string;
      instanceId?: string;
      ownerTerminal?: boolean;
    };
    runningInstanceRuns?: string[];
  } = {},
) {
  const registry = createToolRegistry();
  const calls: Array<{
    destinationId: string;
    message: AutomationInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const listed: Array<{ automationId: string; instanceId: string }> = [];
  const notify: AutomationSignalNotify = async (destinationId, message, topic, idempotencyKey) => {
    const fail =
      typeof options.failNotify === "function"
        ? options.failNotify(destinationId)
        : (options.failNotify ?? false);
    if (fail) throw new Error("notifications down");
    calls.push({ destinationId, message, topic, idempotencyKey });
  };
  registerAutomationTools(registry, {
    findSessionBinding: async () =>
      options.bound === false ? null : (options.binding ?? { runId: RUN_ID }),
    listRunningInstanceRuns: async (automationId, instanceId) => {
      listed.push({ automationId, instanceId });
      return options.runningInstanceRuns ?? [];
    },
    notify,
  });
  const tool = registry.all().find((t) => t.name === "signal_automation");
  if (!tool || tool.handling !== "handled") throw new Error("signal_automation not registered");
  return { registry, tool, calls, listed };
}

describe("signal_automation", () => {
  test("delivers a named signal with payload to the owning run's mailbox", async () => {
    const h = harness();
    const result = await h.tool.handler(
      context(),
      h.tool.input.parse({ signal: "finder_done", payload: { candidates: 3 } }),
    );
    expect(result).toEqual({ delivered: true });
    expect(h.calls).toEqual([
      {
        destinationId: RUN_ID,
        message: {
          kind: "signal",
          name: "finder_done",
          sessionId: "session-1",
          payload: { candidates: 3 },
        },
        topic: AUTOMATION_TOPIC,
        idempotencyKey: `autorun:session-1:signal:finder_done:call-1:${RUN_ID}`,
      },
    ]);
  });

  test("a workstream session's signal fans out to the instance's RUNNING runs, not the terminal creator", async () => {
    // The prod 2026-08-26 shape: the PM session is bound to the long-completed
    // kickoff run, while the daily run (same instance) parks on the signal.
    const daily = "autorun:auto-1:daily:i-inst-1:cron:100";
    const relay = "autorun:auto-1:slack_reply:i-inst-1:slack:Ev1";
    const h = harness({
      binding: {
        runId: RUN_ID,
        automationId: "auto-1",
        instanceId: "inst-1",
        ownerTerminal: true,
      },
      runningInstanceRuns: [daily, relay],
    });
    const result = await h.tool.handler(
      context(),
      h.tool.input.parse({ signal: "review_done", payload: { project_complete: false } }),
    );
    expect(result).toEqual({ delivered: true });
    expect(h.listed).toEqual([{ automationId: "auto-1", instanceId: "inst-1" }]);
    expect(h.calls.map((c) => c.destinationId)).toEqual([daily, relay]);
    // Destination-qualified keys: DBOS notifications dedupe on a GLOBAL
    // message_uuid, so a shared key would drop every destination after the
    // first.
    expect(new Set(h.calls.map((c) => c.idempotencyKey)).size).toBe(2);
    expect(h.calls[0]!.idempotencyKey).toBe(`autorun:session-1:signal:review_done:call-1:${daily}`);
  });

  test("a workstream with no live runs and a terminal creator reports delivered:false", async () => {
    const h = harness({
      binding: {
        runId: RUN_ID,
        automationId: "auto-1",
        instanceId: "inst-1",
        ownerTerminal: true,
      },
      runningInstanceRuns: [],
    });
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "done" }));
    expect(result).toEqual({ delivered: false });
    expect(h.calls).toEqual([]);
  });

  test("a non-instanced session whose owner run already ended sends nothing", async () => {
    const h = harness({ binding: { runId: RUN_ID, ownerTerminal: true } });
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "done" }));
    expect(result).toEqual({ delivered: false });
    expect(h.calls).toEqual([]);
  });

  test("fan-out reports delivered:true when at least one destination accepts", async () => {
    const daily = "autorun:auto-1:daily:i-inst-1:cron:100";
    const relay = "autorun:auto-1:slack_reply:i-inst-1:slack:Ev1";
    const h = harness({
      binding: {
        runId: RUN_ID,
        automationId: "auto-1",
        instanceId: "inst-1",
        ownerTerminal: true,
      },
      runningInstanceRuns: [daily, relay],
      failNotify: (destinationId) => destinationId === daily,
    });
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "done" }));
    expect(result).toEqual({ delivered: true });
    expect(h.calls.map((c) => c.destinationId)).toEqual([relay]);
  });

  test("an unbound session reports delivered:false instead of failing", async () => {
    const h = harness({ bound: false });
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "done" }));
    expect(result).toEqual({ delivered: false });
    expect(h.calls).toEqual([]);
  });

  test("a notification outage never fails the tool", async () => {
    const h = harness({ failNotify: true });
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "done" }));
    expect(result).toEqual({ delivered: false });
  });

  test("rejects malformed signal names and oversized payloads", async () => {
    const h = harness();
    expect(() => h.tool.input.parse({ signal: "Bad-Name" })).toThrow();
    const huge = { blob: "x".repeat(20 * 1024) };
    const result = await h.tool.handler(context(), h.tool.input.parse({ signal: "ok", payload: huge }));
    expect(result).toMatchObject({ error: expect.stringContaining("payload larger") });
  });

  test("is scoped to automation task type in the manifest", () => {
    const h = harness();
    const automation = compileToolManifest(h.registry, [], "automation");
    const chat = compileToolManifest(h.registry, [], "chat");
    expect(automation.some((t) => t.name === "signal_automation")).toBe(true);
    expect(chat.some((t) => t.name === "signal_automation")).toBe(false);
  });
});
