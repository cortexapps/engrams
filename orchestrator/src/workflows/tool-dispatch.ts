/**
 * ToolDispatchWorkflow — the per-session handled-tool pump (ADR 0089 §3).
 *
 * Every task-created session gets one (`workflowID =
 * tooldispatch:<sessionId>[#<epoch>]`). It walks the coordinator's append-only
 * event log in bounded pages and performs only generic tool bookkeeping plus
 * orchestrator-handled tool dispatch. Surface-specific pumps (for example the
 * Slack SessionIngestWorkflow) remain pure consumers and may run alongside it.
 *
 * Effects complete before the local cursor advances. DBOS checkpoints the
 * bounded read and every tool effect, so replay cannot skip an event. A fresh
 * deterministic epoch bounds operation history on long-lived sessions.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { eq } from "drizzle-orm";
import {
  readSessionEventsBounded,
  type BoundedRead,
  type CuratedEvent,
} from "../control-plane/session-events.ts";
import { sessions } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { profile, task, taskSession } from "../db/schema.ts";
import {
  tools as productionTools,
  type SessionToolContext,
  type ToolContext,
  type ToolRegistry,
} from "../tools/registry.ts";
import {
  makePendingToolCallStore,
  type PendingToolCallStore,
} from "../tools/pending-tool-calls.ts";
import {
  completeRegisteredToolCall,
  type ToolCallCompleter,
} from "../tools/complete.ts";

export interface ToolDispatchInput {
  sessionId: string;
  after?: bigint;
  epoch?: number;
}

const POLL_INTERVAL_MS = 1_000;
const RESTART_AFTER_ITERATIONS = 500;

/** Run a non-deterministic tool effect as a durable workflow step. */
export type ToolStepRunner = <T>(fn: () => Promise<T>, name: string) => Promise<T>;

export type { ToolCallCompleter } from "../tools/complete.ts";

export interface ToolDispatchDeps {
  registry: ToolRegistry;
  resolveContext(sessionId: string): Promise<SessionToolContext>;
  pendingCalls: PendingToolCallStore;
  completer: ToolCallCompleter;
}

export interface ToolBookkeepingDeps {
  registry: ToolRegistry;
  pendingCalls: PendingToolCallStore;
  now: () => Date;
}

interface ToolCallRequestedPayload {
  toolCallId: string;
  name: string;
  argsJson?: string;
}

function parseToolCallRequested(event: CuratedEvent): ToolCallRequestedPayload | undefined {
  if (event.kind !== "tool_call_requested") return undefined;
  try {
    const payload = JSON.parse(event.payloadJson) as {
      tool_call_id?: unknown;
      name?: unknown;
      args_json?: unknown;
    };
    if (typeof payload.tool_call_id !== "string" || typeof payload.name !== "string") {
      return undefined;
    }
    return {
      toolCallId: payload.tool_call_id,
      name: payload.name,
      ...(typeof payload.args_json === "string" ? { argsJson: payload.args_json } : {}),
    };
  } catch {
    return undefined;
  }
}

function eventTimestamp(payload: Record<string, unknown>, now: () => Date): Date {
  if (typeof payload.at === "string") {
    const parsed = new Date(payload.at);
    if (!Number.isNaN(parsed.getTime())) return parsed;
  }
  return now();
}

/** Project one generic tool lifecycle event into `pending_tool_calls`. */
export async function bookkeepToolEvent(
  step: ToolStepRunner,
  sessionId: string,
  event: CuratedEvent,
  deps: ToolBookkeepingDeps,
): Promise<void> {
  if (
    event.kind !== "tool_call_requested" &&
    event.kind !== "tool_result_submitted" &&
    event.kind !== "tool_call_completed"
  ) {
    return;
  }
  let payload: Record<string, unknown>;
  try {
    payload = JSON.parse(event.payloadJson) as Record<string, unknown>;
  } catch {
    return;
  }
  const toolCallId = payload.tool_call_id;
  if (typeof toolCallId !== "string") return;
  const at = eventTimestamp(payload, deps.now);

  if (event.kind === "tool_call_requested") {
    if (typeof payload.name !== "string") return;
    const toolName = payload.name;
    // Unknown/stale-manifest names default closed: recording them as handled
    // means the external completion path can never claim them.
    const handling = deps.registry.get(toolName)?.handling ?? "handled";
    await step(
      () =>
        deps.pendingCalls.recordRequested({
          sessionId,
          toolCallId,
          toolName,
          handling,
          requestedAt: at,
        }),
      "record-tool-call-requested",
    );
    return;
  }

  if (event.kind === "tool_result_submitted") {
    await step(
      () => deps.pendingCalls.markSubmitted(toolCallId, at),
      "mark-tool-result-submitted",
    );
    return;
  }

  await step(
    () => deps.pendingCalls.markCompleted(toolCallId, at),
    "mark-tool-call-completed",
  );
}

async function submitToolResult(
  step: ToolStepRunner,
  deps: ToolDispatchDeps,
  sessionId: string,
  toolCallId: string,
  result: unknown,
): Promise<void> {
  await step(
    async () => {
      await completeRegisteredToolCall(
        deps.registry,
        { pendingCalls: deps.pendingCalls, completer: deps.completer },
        sessionId,
        toolCallId,
        result,
      );
    },
    "complete-tool-call",
  );
}

/**
 * Dispatch one curated `tool_call_requested` event when its registered tool is
 * orchestrator-handled. Returns false for non-tool, unknown, and
 * session-handled events.
 */
export async function dispatchHandledToolCall(
  step: ToolStepRunner,
  sessionId: string,
  event: CuratedEvent,
  deps: ToolDispatchDeps,
): Promise<boolean> {
  const requested = parseToolCallRequested(event);
  if (!requested) return false;
  const tool = deps.registry.get(requested.name);
  if (!tool || tool.handling !== "handled") return false;

  let context: ToolContext;
  try {
    const session = await step(() => deps.resolveContext(sessionId), "resolve-tool-context");
    context = { ...session, toolCallId: requested.toolCallId, toolName: tool.name };
  } catch {
    await submitToolResult(
      step,
      deps,
      sessionId,
      requested.toolCallId,
      { error: `session context unavailable for tool ${tool.name}` },
    );
    return true;
  }

  if (tool.capability != null && !context.capabilities.includes(tool.capability)) {
    await submitToolResult(
      step,
      deps,
      sessionId,
      requested.toolCallId,
      { error: `capability denied: ${tool.capability}` },
    );
    return true;
  }

  let rawArgs: unknown;
  try {
    rawArgs = requested.argsJson == null ? undefined : JSON.parse(requested.argsJson);
  } catch {
    rawArgs = undefined;
  }
  const parsedArgs = tool.input.safeParse(rawArgs);
  if (!parsedArgs.success) {
    await submitToolResult(
      step,
      deps,
      sessionId,
      requested.toolCallId,
      { error: `invalid arguments for tool ${tool.name}` },
    );
    return true;
  }

  let rawResult: unknown;
  try {
    rawResult = await step(
      () => Promise.resolve(tool.handler(context, parsedArgs.data)),
      "run-tool-handler",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await submitToolResult(
      step,
      deps,
      sessionId,
      requested.toolCallId,
      { error: `tool ${tool.name} failed: ${message}` },
    );
    return true;
  }

  if (tool.execution === "deferred") return true;

  const parsedResult = tool.output.safeParse(rawResult);
  const result = parsedResult.success
    ? parsedResult.data
    : { error: `invalid result for tool ${tool.name}` };
  await submitToolResult(step, deps, sessionId, requested.toolCallId, result);
  return true;
}

async function resolveToolContext(sessionId: string): Promise<SessionToolContext> {
  const rows = await getDb()
    .select({
      taskId: task.id,
      profileId: taskSession.profileId,
      userId: task.createdByUserId,
      capabilities: profile.capabilities,
    })
    .from(taskSession)
    .innerJoin(task, eq(taskSession.taskId, task.id))
    .leftJoin(profile, eq(taskSession.profileId, profile.id))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(1);
  const row = rows[0];
  if (!row) throw new Error("session task/profile not found");
  return {
    sessionId,
    taskId: row.taskId,
    ...(row.profileId != null ? { profileId: row.profileId } : {}),
    ...(row.userId != null ? { userId: row.userId } : {}),
    capabilities: row.capabilities ?? [],
  };
}

/** Lazy DB-backed store so workflow registration does not touch the database. */
const productionPendingCalls: PendingToolCallStore = {
  recordRequested: (input) => makePendingToolCallStore().recordRequested(input),
  markSubmitted: (toolCallId, at) => makePendingToolCallStore().markSubmitted(toolCallId, at),
  markCompleted: (toolCallId, at) => makePendingToolCallStore().markCompleted(toolCallId, at),
  find: (toolCallId) => makePendingToolCallStore().find(toolCallId),
  listUnsubmittedSessionCallsBefore: (cutoff) =>
    makePendingToolCallStore().listUnsubmittedSessionCallsBefore(cutoff),
};

const productionToolDispatch: ToolDispatchDeps = {
  registry: productionTools,
  resolveContext: resolveToolContext,
  pendingCalls: productionPendingCalls,
  completer: sessions,
};

const productionToolBookkeeping: ToolBookkeepingDeps = {
  registry: productionTools,
  pendingCalls: productionPendingCalls,
  now: () => new Date(),
};

export type ToolBookkeepEffect = (
  step: ToolStepRunner,
  sessionId: string,
  event: CuratedEvent,
) => Promise<void>;

export type HandledToolDispatchEffect = (
  step: ToolStepRunner,
  sessionId: string,
  event: CuratedEvent,
) => Promise<boolean>;

export interface ToolDispatchWorkflowDeps {
  step: ToolStepRunner;
  readPage(sessionId: string, after: bigint): Promise<BoundedRead>;
  bookkeep: ToolBookkeepEffect;
  dispatch: HandledToolDispatchEffect;
  sleep(ms: number): Promise<void>;
  restart(input: ToolDispatchInput, workflowId: string): Promise<void>;
}

/** Testable dispatch-pump loop; production supplies only checkpointed seams. */
export async function toolDispatchWorkflowImpl(
  input: ToolDispatchInput,
  deps: ToolDispatchWorkflowDeps,
): Promise<void> {
  const { sessionId } = input;
  let after = input.after ?? -1n;
  const epoch = input.epoch ?? 0;

  for (let i = 0; i < RESTART_AFTER_ITERATIONS; i++) {
    const page = await deps.readPage(sessionId, after);

    // Effect-before-cursor: both durable effects finish before `after` moves.
    for (const event of page.events) {
      await deps.bookkeep(deps.step, sessionId, event);
      await deps.dispatch(deps.step, sessionId, event);
    }
    after = page.nextAfter;

    if (page.terminal) return;

    if (page.events.length === 0) {
      await deps.sleep(POLL_INTERVAL_MS);
    }
  }

  const nextEpoch = epoch + 1;
  await deps.restart(
    { sessionId, after, epoch: nextEpoch },
    `tooldispatch:${sessionId}#${nextEpoch}`,
  );
}

const readPage = DBOS.registerStep(
  (sessionId: string, after: bigint) => readSessionEventsBounded(sessionId, after),
  { name: "tool-dispatch-read-page" },
);

async function toolDispatchWorkflowEntry(input: ToolDispatchInput): Promise<void> {
  await toolDispatchWorkflowImpl(input, {
    step: (fn, name) => DBOS.runStep(fn, { name }),
    readPage,
    bookkeep: (step, sessionId, event) =>
      bookkeepToolEvent(step, sessionId, event, productionToolBookkeeping),
    dispatch: (step, sessionId, event) =>
      dispatchHandledToolCall(step, sessionId, event, productionToolDispatch),
    sleep: (ms) => DBOS.sleep(ms),
    restart: async (nextInput, workflowId) => {
      await DBOS.startWorkflow(toolDispatchWorkflow, { workflowID: workflowId })(nextInput);
    },
  });
}

export const toolDispatchWorkflow = DBOS.registerWorkflow(toolDispatchWorkflowEntry, {
  name: "ToolDispatchWorkflow",
});

/** Start the epoch-zero dispatch pump for a newly persisted session. */
export async function startToolDispatchWorkflow(sessionId: string): Promise<void> {
  await DBOS.startWorkflow(toolDispatchWorkflow, {
    workflowID: `tooldispatch:${sessionId}`,
  })({ sessionId });
}
