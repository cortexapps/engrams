import { describe, expect, test } from "bun:test";
import { z } from "zod";

import { registerBuiltinTools } from "../../tools/builtin.ts";
import { createToolRegistry } from "../../tools/registry.ts";
import type {
  PendingToolCallInput,
  PendingToolCallStore,
} from "../../tools/pending-tool-calls.ts";
import {
  makeToolConsumer,
  type ToolWorkflowStarter,
} from "../tool-consumer.ts";

function pendingRecorder() {
  const requested: PendingToolCallInput[] = [];
  const submitted: Array<{ toolCallId: string; at: Date }> = [];
  const completed: Array<{ toolCallId: string; at: Date }> = [];
  const store: PendingToolCallStore = {
    recordRequested: async (input) => void requested.push(input),
    markSubmitted: async (toolCallId, at) => void submitted.push({ toolCallId, at }),
    markCompleted: async (toolCallId, at) => void completed.push({ toolCallId, at }),
    find: async () => null,
    listUnsubmittedSessionCallsBefore: async () => [],
  };
  return { store, requested, submitted, completed };
}

function requested(name: string, toolCallId = "call-1") {
  return {
    idx: 1n,
    kind: "tool_call_requested",
    payloadJson: JSON.stringify({
      tool_call_id: toolCallId,
      name,
      args_json: JSON.stringify({ text: "remember" }),
      at: "2026-07-13T12:00:00.000Z",
    }),
  };
}

function registryFixture() {
  const registry = createToolRegistry();
  registry.register({
    name: "save_memory",
    description: "Save a note",
    input: z.object({ text: z.string() }),
    output: z.object({ saved: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async () => ({ saved: true }),
  });
  registerBuiltinTools(registry);
  return registry;
}

describe("tool consumer", () => {
  test("handled requests start the same toolexec workflow ID on redelivery", async () => {
    const pending = pendingRecorder();
    const workflowIds: string[] = [];
    const starter: ToolWorkflowStarter = async (_input, workflowId) => {
      workflowIds.push(workflowId);
    };
    const consumer = makeToolConsumer({
      registry: registryFixture(),
      pendingCalls: pending.store,
      startWorkflow: starter,
      now: () => new Date(0),
    });
    const ev = requested("save_memory");

    await consumer.handle(ev, { sessionId: "session-1" });
    await consumer.handle(ev, { sessionId: "session-1" });

    expect(workflowIds).toEqual(["toolexec:call-1", "toolexec:call-1"]);
  });

  test("ask_user_question is bookkept as session-handled and never dispatched", async () => {
    const pending = pendingRecorder();
    const workflowIds: string[] = [];
    const consumer = makeToolConsumer({
      registry: registryFixture(),
      pendingCalls: pending.store,
      startWorkflow: async (_input, workflowId) => void workflowIds.push(workflowId),
      now: () => new Date(0),
    });

    await consumer.handle(requested("ask_user_question", "call-session"), {
      sessionId: "session-1",
    });
    expect(workflowIds).toEqual([]);
    expect(pending.requested.map((row) => [row.toolCallId, row.handling])).toEqual([
      ["call-session", "session"],
    ]);
  });

  test("unknown tools run bookkeeping without dispatch", async () => {
    const pending = pendingRecorder();
    const workflowIds: string[] = [];
    const consumer = makeToolConsumer({
      registry: registryFixture(),
      pendingCalls: pending.store,
      startWorkflow: async (_input, workflowId) => void workflowIds.push(workflowId),
      now: () => new Date(0),
    });

    await consumer.handle(requested("stale_manifest_tool", "call-stale"), {
      sessionId: "session-1",
    });

    expect(workflowIds).toEqual([]);
    expect(pending.requested.map((row) => [row.toolCallId, row.handling])).toEqual([
      ["call-stale", "handled"],
    ]);
  });

  test("bookkeeps requested, result-submitted, and completed events", async () => {
    const pending = pendingRecorder();
    const consumer = makeToolConsumer({
      registry: registryFixture(),
      pendingCalls: pending.store,
      startWorkflow: async () => {},
      now: () => new Date(0),
    });

    await consumer.handle(requested("ask_user_question"), { sessionId: "session-1" });
    await consumer.handle({
      idx: 2n,
      kind: "tool_result_submitted",
      payloadJson: JSON.stringify({
        tool_call_id: "call-1",
        at: "2026-07-13T12:01:00.000Z",
      }),
    }, { sessionId: "session-1" });
    await consumer.handle({
      idx: 3n,
      kind: "tool_call_completed",
      payloadJson: JSON.stringify({
        tool_call_id: "call-1",
        at: "2026-07-13T12:02:00.000Z",
      }),
    }, { sessionId: "session-1" });

    expect(pending.requested[0]).toEqual({
      sessionId: "session-1",
      toolCallId: "call-1",
      toolName: "ask_user_question",
      handling: "session",
      requestedAt: new Date("2026-07-13T12:00:00.000Z"),
    });
    expect(pending.submitted).toEqual([
      { toolCallId: "call-1", at: new Date("2026-07-13T12:01:00.000Z") },
    ]);
    expect(pending.completed).toEqual([
      { toolCallId: "call-1", at: new Date("2026-07-13T12:02:00.000Z") },
    ]);
  });
});
