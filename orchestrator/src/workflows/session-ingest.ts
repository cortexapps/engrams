/**
 * SessionIngestWorkflow — the reverse-channel pump (ADR 0060 Decision 1, P1.3).
 *
 * One per session (`workflowID = ingest:<sessionId>[#<epoch>]`, which IS the
 * one-pump-per-session guarantee — no lease). It walks the coordinator's
 * append-only event log forward in bounded reads via `readSessionEventsBounded`
 * and `DBOS.send`s curated content into the owning `SlackThreadWorkflow`'s
 * mailbox, then sends a single terminal message and exits when the session
 * reaches a terminal state.
 *
 * Two invariants this file pins (ADR 0060 §Correctness invariants):
 *  1. **Effect-before-cursor.** Each `DBOS.send` (a checkpointed, replay-once
 *     step) commits BEFORE the local cursor `after` advances. A crash in between
 *     replays the send, which DBOS returns from its checkpoint — no re-send, no
 *     gap. That is what makes the workflow-local cursor safe without a table.
 *  2. **`run_completed` is not terminal.** Only a terminal `status_changed`
 *     (surfaced as `page.terminal` by the reader) ends the loop — a session
 *     re-runs on a follow-up @mention.
 *
 * No `continueAsNew` in the DBOS SDK (divergence from the ADR sketch): to bound
 * `operation_outputs` growth on a long session we self-restart — start a fresh
 * epoch with a deterministic `ingest:<sid>#<epoch+1>` id carrying the cursor,
 * then return. The deterministic id makes the restart idempotent under replay.
 *
 * ADR 0089 handled-tool failures are completed with exactly this JSON shape in
 * `result_json`: `{"error":"<message>"}`. The coordinator treats that JSON as
 * opaque; keeping one stable envelope lets every harness surface failures.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { eq } from "drizzle-orm";
import { readSessionEventsBounded } from "../control-plane/session-events.ts";
import type { CuratedEvent } from "../control-plane/session-events.ts";
import { sessions } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { profile, task, taskSession } from "../db/schema.ts";
import {
  tools as productionTools,
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
import { THREAD_TOPIC, type ThreadInbox } from "./thread-inbox.ts";

/** Workflow input. `after`/`epoch`/`lastMessage` are set only on a self-restart
 *  (the cursor, epoch, and last-seen assistant message the prior pump handed
 *  off); a fresh pump starts at the log head. */
export interface IngestInput {
  sessionId: string;
  threadWfId: string;
  after?: bigint;
  epoch?: number;
  lastMessage?: string;
}

/** Poll cadence when a bounded read returned no new curated content (tail). */
const POLL_INTERVAL_MS = 1_000;
/** Self-restart after this many iterations to bound `operation_outputs`. */
const RESTART_AFTER_ITERATIONS = 500;

/** Run a non-deterministic tool effect as a durable workflow step. Tests pass
 *  an inline runner, mirroring slack-thread's `handleInbound` seam. */
export type ToolStepRunner = <T>(fn: () => Promise<T>, name: string) => Promise<T>;

/** Structural subset of SessionService used to return a tool result. */
export type { ToolCallCompleter } from "../tools/complete.ts";

export interface ToolDispatchDeps {
  registry: ToolRegistry;
  resolveContext(sessionId: string): Promise<ToolContext>;
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
 * orchestrator-handled. Returns true when the event named a handled tool and
 * false for non-tool, unknown, and session-handled events. Session-handled
 * calls deliberately continue to policy/surfaces via the normal ingest send.
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
    context = await step(() => deps.resolveContext(sessionId), "resolve-tool-context");
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

  // A deferred handler only starts the operation; its later callback uses
  // tools.complete(). Sync handlers return and complete in this dispatch.
  if (tool.execution === "deferred") return true;

  const parsedResult = tool.output.safeParse(rawResult);
  const result = parsedResult.success
    ? parsedResult.data
    : { error: `invalid result for tool ${tool.name}` };
  await submitToolResult(step, deps, sessionId, requested.toolCallId, result);
  return true;
}

/** Load the task/profile context that authorizes a handled tool invocation. */
async function resolveToolContext(sessionId: string): Promise<ToolContext> {
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

/** Lazy DB-backed store: module import registers DBOS workflows without
 *  requiring a database connection; getDb() is reached only inside a step. */
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

/**
 * One bounded read of the log, as a checkpointed step: non-deterministic
 * (reads external state) by design, so its result is recorded once and
 * returned verbatim on replay.
 */
const readPage = DBOS.registerStep(
  (sessionId: string, after: bigint) => readSessionEventsBounded(sessionId, after),
  { name: "ingest-read-page" },
);

async function sessionIngestWorkflowImpl(input: IngestInput): Promise<void> {
  const { sessionId, threadWfId } = input;
  let after: bigint = input.after ?? -1n;
  const epoch = input.epoch ?? 0;
  // The session's most-recent assistant message seen so far — carried across a
  // self-restart so the closing summary stays correct even past a history bound.
  let lastMessage: string | undefined = input.lastMessage;
  const toolStep: ToolStepRunner = (fn, name) => DBOS.runStep(fn, { name });

  for (let i = 0; i < RESTART_AFTER_ITERATIONS; i++) {
    const page = await readPage(sessionId, after);

    // Effect-before-cursor: forward each curated event (replay-once send)
    // BEFORE advancing the cursor.
    for (const ev of page.events) {
      await bookkeepToolEvent(toolStep, sessionId, ev, productionToolBookkeeping);
      await dispatchHandledToolCall(toolStep, sessionId, ev, productionToolDispatch);
      await DBOS.send<ThreadInbox>(threadWfId, { kind: "session_event", event: ev }, THREAD_TOPIC);
    }
    after = page.nextAfter;
    if (page.lastAssistantText !== undefined) lastMessage = page.lastAssistantText;

    if (page.terminal) {
      await DBOS.send<ThreadInbox>(
        threadWfId,
        { kind: "session_terminal", outcome: page.terminal.outcome, ...(lastMessage ? { lastMessage } : {}) },
        THREAD_TOPIC,
      );
      return;
    }

    // At the tail (no new content) — wait before the next bounded read.
    if (page.events.length === 0) {
      await DBOS.sleep(POLL_INTERVAL_MS);
    }
  }

  // History bound reached: hand the cursor to a fresh epoch and exit. The
  // deterministic id makes this restart idempotent under replay.
  await DBOS.startWorkflow(sessionIngestWorkflow, {
    workflowID: `ingest:${sessionId}#${epoch + 1}`,
  })({ sessionId, threadWfId, after, epoch: epoch + 1, ...(lastMessage ? { lastMessage } : {}) });
}

export const sessionIngestWorkflow = DBOS.registerWorkflow(sessionIngestWorkflowImpl, {
  name: "SessionIngestWorkflow",
});
