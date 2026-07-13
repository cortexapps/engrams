/**
 * ADR 0089 handled-tool dispatch. The production dispatch workflow supplies a
 * DBOS StepRunner; these tests run steps inline and inject recording seams, so
 * no DBOS engine, coordinator, or database is started.
 */

import { describe, expect, test } from "bun:test";
import { z } from "zod";

import {
  readSessionEventsBounded,
  type ListEventsFn,
} from "../../control-plane/session-events.ts";
import {
  createToolRegistry,
  type SessionToolContext,
  type ToolContext,
} from "../../tools/registry.ts";
import type {
  PendingToolCallInput,
  PendingToolCallRow,
  PendingToolCallStore,
} from "../../tools/pending-tool-calls.ts";
import {
  bookkeepToolEvent,
  dispatchHandledToolCall,
  type ToolCallCompleter,
  type ToolDispatchDeps,
  type ToolStepRunner,
  toolDispatchWorkflowImpl,
} from "../tool-dispatch.ts";

const STEP: ToolStepRunner = (fn) => fn();

const EVENT = {
  idx: 9n,
  kind: "tool_call_requested",
  payloadJson: JSON.stringify({
    run_id: "run-1",
    tool_call_id: "call-1",
    name: "save_memory",
    args_json: JSON.stringify({ text: "remember this" }),
  }),
};

describe("toolDispatchWorkflowImpl", () => {
  test("processes tool effects before advancing the cursor and exits on terminal", async () => {
    const reads: bigint[] = [];
    const effects: Array<{ kind: "bookkeep" | "dispatch"; idx: bigint }> = [];
    const sleeps: number[] = [];
    const restarts: string[] = [];
    const listEvents: ListEventsFn = async (_sessionId, after) => {
      reads.push(after);
      if (reads.length === 1) {
        return { events: [EVENT], nextAfterIdx: EVENT.idx };
      }
      if (reads.length === 2) {
        return {
          events: [
            {
              idx: 10n,
              kind: "status_changed",
              payloadJson: JSON.stringify({ to: "completed" }),
            },
          ],
          nextAfterIdx: 10n,
        };
      }
      throw new Error("dispatch pump read past terminal");
    };

    await toolDispatchWorkflowImpl(
      { sessionId: "session-1" },
      {
        step: STEP,
        readPage: (sessionId, after) =>
          readSessionEventsBounded(sessionId, after, listEvents),
        bookkeep: async (_step, _sessionId, event) => {
          effects.push({ kind: "bookkeep", idx: event.idx });
        },
        dispatch: async (_step, _sessionId, event) => {
          effects.push({ kind: "dispatch", idx: event.idx });
          return true;
        },
        sleep: async (ms) => {
          sleeps.push(ms);
        },
        restart: async (_input, workflowId) => {
          restarts.push(workflowId);
        },
      },
    );

    expect(effects).toEqual([
      { kind: "bookkeep", idx: 9n },
      { kind: "dispatch", idx: 9n },
    ]);
    expect(reads).toEqual([-1n, 9n]);
    expect(sleeps).toEqual([]);
    expect(restarts).toEqual([]);
  });
});

function recordingCompleter() {
  const calls: Array<{ sessionId: string; toolCallId: string; resultJson: string }> = [];
  const completer: ToolCallCompleter = {
    completeToolCall: async (request) => void calls.push(request),
  };
  return { completer, calls };
}

function pendingCallStore(toolName = "save_memory"): PendingToolCallStore {
  return {
    recordRequested: async () => {},
    markSubmitted: async () => {},
    markCompleted: async () => {},
    find: async (toolCallId) => ({
      sessionId: "session-1",
      toolCallId,
      toolName,
      handling: toolName === "ask_user_question" ? "session" : "handled",
      requestedAt: new Date(0),
      submittedAt: null,
      completedAt: null,
    }),
    listUnsubmittedSessionCallsBefore: async () => [],
  };
}

function handledDeps(options: {
  capabilities?: string[];
  handler?: (ctx: ToolContext, args: { text: string }) => unknown;
  output?: z.ZodType;
} = {}) {
  const registry = createToolRegistry();
  const handlerCalls: Array<{ ctx: ToolContext; args: { text: string } }> = [];
  registry.register({
    name: "save_memory",
    description: "Save a note.",
    input: z.object({ text: z.string() }),
    output: options.output ?? z.object({ saved: z.boolean() }),
    handling: "handled",
    execution: "sync",
    capability: "memory:write",
    handler: async (ctx, args) => {
      handlerCalls.push({ ctx, args });
      return options.handler?.(ctx, args) ?? { saved: true };
    },
  });
  const completion = recordingCompleter();
  const context: SessionToolContext = {
    sessionId: "session-1",
    taskId: "task-1",
    profileId: "profile-1",
    userId: "user-1",
    capabilities: options.capabilities ?? ["memory:write"],
  };
  const deps: ToolDispatchDeps = {
    registry,
    resolveContext: async () => context,
    pendingCalls: pendingCallStore(),
    completer: completion.completer,
  };
  return { deps, context, handlerCalls, completionCalls: completion.calls };
}

describe("dispatchHandledToolCall", () => {
  test("capability denial completes with an error and never runs the handler", async () => {
    const { deps, handlerCalls, completionCalls } = handledDeps({ capabilities: [] });

    const dispatched = await dispatchHandledToolCall(STEP, "session-1", EVENT, deps);

    expect(dispatched).toBe(true);
    expect(handlerCalls).toEqual([]);
    expect(completionCalls).toEqual([
      {
        sessionId: "session-1",
        toolCallId: "call-1",
        resultJson: JSON.stringify({ error: "capability denied: memory:write" }),
      },
    ]);
  });

  test("invalid args complete with an error and never run the handler", async () => {
    const { deps, handlerCalls, completionCalls } = handledDeps();
    const invalid = {
      ...EVENT,
      payloadJson: JSON.stringify({
        run_id: "run-1",
        tool_call_id: "call-1",
        name: "save_memory",
        args_json: JSON.stringify({ text: 42 }),
      }),
    };

    await dispatchHandledToolCall(STEP, "session-1", invalid, deps);

    expect(handlerCalls).toEqual([]);
    expect(JSON.parse(completionCalls[0]!.resultJson)).toEqual({
      error: "invalid arguments for tool save_memory",
    });
  });

  test("runs a valid handler with session context and completes its validated result", async () => {
    const { deps, context, handlerCalls, completionCalls } = handledDeps();

    const dispatched = await dispatchHandledToolCall(STEP, "session-1", EVENT, deps);

    expect(dispatched).toBe(true);
    // The context carries the call identity so a deferred handler can later
    // call tools.complete(ctx.sessionId, ctx.toolCallId, result).
    const expectedCtx: ToolContext = {
      ...context,
      toolCallId: "call-1",
      toolName: "save_memory",
    };
    expect(handlerCalls).toEqual([{ ctx: expectedCtx, args: { text: "remember this" } }]);
    expect(completionCalls).toEqual([
      {
        sessionId: "session-1",
        toolCallId: "call-1",
        resultJson: JSON.stringify({ saved: true }),
      },
    ]);
  });

  test("invalid handler output completes with an error result", async () => {
    const { deps, completionCalls } = handledDeps({ handler: () => ({ saved: "yes" }) });

    await dispatchHandledToolCall(STEP, "session-1", EVENT, deps);

    expect(JSON.parse(completionCalls[0]!.resultJson)).toEqual({
      error: "invalid result for tool save_memory",
    });
  });

  test("session-handled tools are not dispatched or completed by the tool pump", async () => {
    const registry = createToolRegistry();
    registry.register({
      name: "ask_user_question",
      description: "Ask a question.",
      input: z.object({ question: z.string() }),
      output: z.object({ answer: z.string() }),
      handling: "session",
      presenters: { web: "QuestionCard" },
    });
    const completion = recordingCompleter();
    let contextCalls = 0;
    const event = {
      ...EVENT,
      payloadJson: JSON.stringify({
        run_id: "run-1",
        tool_call_id: "call-q",
        name: "ask_user_question",
        args_json: JSON.stringify({ question: "Ship?" }),
      }),
    };

    const dispatched = await dispatchHandledToolCall(STEP, "session-1", event, {
      registry,
      resolveContext: async () => {
        contextCalls++;
        return { sessionId: "session-1", capabilities: [] };
      },
      pendingCalls: pendingCallStore("ask_user_question"),
      completer: completion.completer,
    });

    expect(dispatched).toBe(false);
    expect(contextCalls).toBe(0);
    expect(completion.calls).toEqual([]);
  });
});

function recordingPendingCalls() {
  const requested: PendingToolCallInput[] = [];
  const submitted: Array<{ toolCallId: string; at: Date }> = [];
  const completed: Array<{ toolCallId: string; at: Date }> = [];
  const store: PendingToolCallStore = {
    recordRequested: async (input) => void requested.push(input),
    markSubmitted: async (toolCallId, at) => void submitted.push({ toolCallId, at }),
    markCompleted: async (toolCallId, at) => void completed.push({ toolCallId, at }),
    find: async () => null,
    listUnsubmittedSessionCallsBefore: async () => [] as PendingToolCallRow[],
  };
  return { store, requested, submitted, completed };
}

describe("bookkeepToolEvent", () => {
  test("records requested, submitted, and completed tool-call lifecycle timestamps", async () => {
    const registry = createToolRegistry();
    registry.register({
      name: "ask_user_question",
      description: "Ask a question.",
      input: z.object({ question: z.string() }),
      output: z.object({ answer: z.string() }),
      handling: "session",
      presenters: { web: "QuestionCard" },
    });
    const pending = recordingPendingCalls();
    const deps = { registry, pendingCalls: pending.store, now: () => new Date(0) };

    await bookkeepToolEvent(STEP, "session-1", {
      idx: 1n,
      kind: "tool_call_requested",
      payloadJson: JSON.stringify({
        tool_call_id: "call-1",
        name: "ask_user_question",
        args_json: "{}",
        at: "2026-07-13T12:00:00.000Z",
      }),
    }, deps);
    await bookkeepToolEvent(STEP, "session-1", {
      idx: 2n,
      kind: "tool_result_submitted",
      payloadJson: JSON.stringify({
        tool_call_id: "call-1",
        result_json: "{}",
        at: "2026-07-13T12:01:00.000Z",
      }),
    }, deps);
    await bookkeepToolEvent(STEP, "session-1", {
      idx: 3n,
      kind: "tool_call_completed",
      payloadJson: JSON.stringify({
        tool_call_id: "call-1",
        tool_name: "ask_user_question",
        at: "2026-07-13T12:02:00.000Z",
      }),
    }, deps);

    expect(pending.requested).toEqual([
      {
        sessionId: "session-1",
        toolCallId: "call-1",
        toolName: "ask_user_question",
        handling: "session",
        requestedAt: new Date("2026-07-13T12:00:00.000Z"),
      },
    ]);
    expect(pending.submitted).toEqual([
      { toolCallId: "call-1", at: new Date("2026-07-13T12:01:00.000Z") },
    ]);
    expect(pending.completed).toEqual([
      { toolCallId: "call-1", at: new Date("2026-07-13T12:02:00.000Z") },
    ]);
  });
});
