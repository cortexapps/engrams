import { describe, expect, test } from "bun:test";
import { z } from "zod";

import type { ToolCallCompleter } from "../complete.ts";
import {
  executeToolCall,
  submitToolExecution,
  type ToolExecDeps,
  type ToolExecInput,
} from "../exec.ts";
import type { PendingToolCallStore } from "../pending-tool-calls.ts";
import {
  createToolRegistry,
  type ToolContext,
} from "../registry.ts";

const INPUT: ToolExecInput = {
  sessionId: "session-1",
  toolCallId: "call-1",
  toolName: "save_memory",
  argsJson: JSON.stringify({ text: "remember" }),
};

function fixture(options: {
  execution?: "sync" | "deferred";
  capabilities?: string[];
  argsJson?: string;
  output?: z.ZodType;
  handler?: (ctx: ToolContext, args: { text: string }) => unknown;
  complete?: ToolCallCompleter["completeToolCall"];
} = {}) {
  const registry = createToolRegistry();
  const handlerCalls: Array<{ ctx: ToolContext; args: { text: string } }> = [];
  registry.register({
    name: "save_memory",
    description: "Save a note",
    input: z.object({ text: z.string() }),
    output: options.output ?? z.object({ saved: z.boolean() }),
    handling: "handled",
    execution: options.execution ?? "sync",
    capability: "memory:write",
    handler: async (ctx, args) => {
      handlerCalls.push({ ctx, args });
      return options.handler?.(ctx, args) ?? { saved: true };
    },
  });
  const completionCalls: Array<{
    sessionId: string;
    toolCallId: string;
    resultJson: string;
  }> = [];
  const submitted: string[] = [];
  const pending: PendingToolCallStore = {
    recordRequested: async () => {},
    markSubmitted: async (sessionId, toolCallId) =>
      void submitted.push(`${sessionId}:${toolCallId}`),
    markCompleted: async () => {},
    find: async (sessionId, toolCallId) => ({
      sessionId,
      toolCallId,
      toolName: "save_memory",
      handling: "handled",
      requestedAt: new Date(0),
      submittedAt: null,
      completedAt: null,
    }),
    listSessionIdsWithPendingSessionCalls: async () => [],
    listUnsubmittedSessionCallsBefore: async () => [],
  };
  const completer: ToolCallCompleter = {
    completeToolCall: options.complete ?? (async (request) => {
      completionCalls.push(request);
    }),
  };
  const deps: ToolExecDeps = {
    registry,
    resolveContext: async () => ({
      sessionId: "session-1",
      taskId: "task-1",
      capabilities: options.capabilities ?? ["memory:write"],
    }),
    pendingCalls: pending,
    completer,
    now: () => new Date(0),
    nowMs: () => 100,
  };
  return {
    deps,
    input: { ...INPUT, ...(options.argsJson ? { argsJson: options.argsJson } : {}) },
    handlerCalls,
    completionCalls,
    submitted,
  };
}

async function runAndSubmit(f: ReturnType<typeof fixture>) {
  const outcome = await executeToolCall(f.input, f.deps);
  await submitToolExecution(f.input, outcome, f.deps);
  return outcome;
}

describe("tool exec plain functions", () => {
  test("sync tool runs its handler and submits the validated result", async () => {
    const f = fixture();

    await runAndSubmit(f);

    expect(f.handlerCalls).toHaveLength(1);
    expect(JSON.parse(f.completionCalls[0]!.resultJson)).toEqual({ saved: true });
  });

  test("deferred tool runs its handler without submitting", async () => {
    const f = fixture({ execution: "deferred" });

    const outcome = await runAndSubmit(f);

    expect(outcome).toEqual({ kind: "deferred" });
    expect(f.handlerCalls).toHaveLength(1);
    expect(f.completionCalls).toEqual([]);
  });

  test("handler failure is submitted as the exact protocol error envelope", async () => {
    const f = fixture({ handler: () => { throw new Error("disk full"); } });

    await runAndSubmit(f);

    expect(JSON.parse(f.completionCalls[0]!.resultJson)).toEqual({
      error: "tool save_memory failed: disk full",
    });
  });

  test("handler-returned protocol errors bypass the declared success schema", async () => {
    const f = fixture({
      handler: () => ({ error: "no active review for this session" }),
    });

    await runAndSubmit(f);

    expect(JSON.parse(f.completionCalls[0]!.resultJson)).toEqual({
      error: "no active review for this session",
    });
  });

  test("missing session context is submitted as the exact protocol error envelope", async () => {
    const f = fixture();
    f.deps.resolveContext = async () => {
      throw new Error("profile row disappeared");
    };

    await runAndSubmit(f);

    expect(JSON.parse(f.completionCalls[0]!.resultJson)).toEqual({
      error: "session context unavailable for tool save_memory",
    });
    expect(f.handlerCalls).toEqual([]);
  });

  test("capability denial, invalid args, and invalid result use exact envelopes", async () => {
    const denied = fixture({ capabilities: [] });
    await runAndSubmit(denied);
    expect(JSON.parse(denied.completionCalls[0]!.resultJson)).toEqual({
      error: "capability denied: memory:write",
    });

    const invalidArgs = fixture({ argsJson: JSON.stringify({ text: 42 }) });
    await runAndSubmit(invalidArgs);
    const invalidArgsResult = JSON.parse(
      invalidArgs.completionCalls[0]!.resultJson,
    ) as { error: string };
    // Names the offending field and expected shape so the model can fix the
    // call without a guess-and-retry round trip (papercut 2026-07-21).
    expect(invalidArgsResult.error).toStartWith(
      "invalid arguments for tool save_memory: text:",
    );
    expect(invalidArgsResult.error).toContain("string");

    const invalidResult = fixture({ handler: () => ({ saved: "yes" }) });
    await runAndSubmit(invalidResult);
    expect(JSON.parse(invalidResult.completionCalls[0]!.resultJson)).toEqual({
      error: "invalid result for tool save_memory",
    });
  });

  test("an already-completed submit is idempotent success", async () => {
    const f = fixture({
      complete: async () => {
        throw new Error("tool call already completed");
      },
    });
    const outcome = await executeToolCall(f.input, f.deps);

    await expect(submitToolExecution(f.input, outcome, f.deps)).resolves.toBeUndefined();
    expect(f.submitted).toEqual(["session-1:call-1"]);
  });
});
