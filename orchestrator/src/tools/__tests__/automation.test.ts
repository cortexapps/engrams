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

function harness(options: { bound?: boolean; failNotify?: boolean } = {}) {
  const registry = createToolRegistry();
  const calls: Array<{
    destinationId: string;
    message: AutomationInbox;
    topic: string;
    idempotencyKey: string;
  }> = [];
  const notify: AutomationSignalNotify = async (destinationId, message, topic, idempotencyKey) => {
    if (options.failNotify) throw new Error("notifications down");
    calls.push({ destinationId, message, topic, idempotencyKey });
  };
  registerAutomationTools(registry, {
    findSessionBinding: async () => (options.bound === false ? null : { runId: RUN_ID }),
    notify,
  });
  const tool = registry.all().find((t) => t.name === "signal_automation");
  if (!tool || tool.handling !== "handled") throw new Error("signal_automation not registered");
  return { registry, tool, calls };
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
        idempotencyKey: "autorun:session-1:signal:finder_done:call-1",
      },
    ]);
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
