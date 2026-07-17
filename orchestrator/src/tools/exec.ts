import { Code, ConnectError } from "@connectrpc/connect";
import { eq } from "drizzle-orm";

import { sessions } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { profile, task, taskSession } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import {
  completeRegisteredToolCall,
  isProtocolErrorResult,
  type ToolCallCompleter,
} from "./complete.ts";
import {
  makePendingToolCallStore,
  type PendingToolCallStore,
} from "./pending-tool-calls.ts";
import {
  tools as productionTools,
  type SessionToolContext,
  type ToolContext,
  type ToolRegistry,
} from "./registry.ts";

const log = rootLog.child({ component: "tool-exec" });

export interface ToolExecInput {
  sessionId: string;
  toolCallId: string;
  toolName: string;
  argsJson?: string;
}

export type ToolExecutionOutcome =
  | { kind: "deferred" }
  | { kind: "submit"; result: unknown };

export interface ToolExecDeps {
  registry: ToolRegistry;
  resolveContext(sessionId: string): Promise<SessionToolContext>;
  pendingCalls: PendingToolCallStore;
  completer: ToolCallCompleter;
  now: () => Date;
  nowMs: () => number;
}

/** Resolve the session-owned task/profile context used for capability gates. */
export async function resolveToolContext(
  sessionId: string,
): Promise<SessionToolContext> {
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
    ...(row.profileId !== null ? { profileId: row.profileId } : {}),
    ...(row.userId !== null ? { userId: row.userId } : {}),
    capabilities: row.capabilities ?? [],
  };
}

/** Resolve, gate, validate, and invoke a handled tool. No DBOS APIs live here. */
export async function executeToolCall(
  input: ToolExecInput,
  deps: ToolExecDeps,
): Promise<ToolExecutionOutcome> {
  const startedAt = deps.nowMs();
  log.info(
    {
      sessionId: input.sessionId,
      toolCallId: input.toolCallId,
      toolName: input.toolName,
    },
    "tool exec start",
  );
  try {
    const tool = deps.registry.get(input.toolName);
    if (!tool || tool.handling !== "handled") {
      throw new Error(`handled tool is not registered: ${input.toolName}`);
    }

    let sessionContext: SessionToolContext;
    try {
      sessionContext = await deps.resolveContext(input.sessionId);
    } catch {
      return {
        kind: "submit",
        result: { error: `session context unavailable for tool ${tool.name}` },
      };
    }
    const context: ToolContext = {
      ...sessionContext,
      toolCallId: input.toolCallId,
      toolName: tool.name,
    };

    if (
      tool.capability !== undefined &&
      !context.capabilities.includes(tool.capability)
    ) {
      return {
        kind: "submit",
        result: { error: `capability denied: ${tool.capability}` },
      };
    }

    let rawArgs: unknown;
    try {
      rawArgs = input.argsJson === undefined
        ? undefined
        : JSON.parse(input.argsJson);
    } catch {
      rawArgs = undefined;
    }
    const parsedArgs = tool.input.safeParse(rawArgs);
    if (!parsedArgs.success) {
      return {
        kind: "submit",
        result: { error: `invalid arguments for tool ${tool.name}` },
      };
    }

    let rawResult: unknown;
    try {
      rawResult = await tool.handler(context, parsedArgs.data);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        kind: "submit",
        result: { error: `tool ${tool.name} failed: ${message}` },
      };
    }
    if (tool.execution === "deferred") return { kind: "deferred" };

    const parsedResult = tool.output.safeParse(rawResult);
    return {
      kind: "submit",
      result: isProtocolErrorResult(rawResult)
        ? rawResult
        : parsedResult.success
        ? parsedResult.data
        : { error: `invalid result for tool ${tool.name}` },
    };
  } finally {
    log.info(
      {
        sessionId: input.sessionId,
        toolCallId: input.toolCallId,
        toolName: input.toolName,
        durationMs: deps.nowMs() - startedAt,
      },
      "tool exec finish",
    );
  }
}

function alreadyCompleted(error: unknown): boolean {
  if (error instanceof ConnectError && error.code === Code.AlreadyExists) {
    return true;
  }
  const message = error instanceof Error ? error.message : String(error);
  return /duplicate|already.*(?:completed|submitted|result|exists)/i.test(message);
}

/** Submit a synchronous execution result, accepting coordinator duplicate
 * completion as success during workflow replay. */
export async function submitToolExecution(
  input: ToolExecInput,
  outcome: ToolExecutionOutcome,
  deps: ToolExecDeps,
): Promise<void> {
  if (outcome.kind === "deferred") return;
  try {
    await completeRegisteredToolCall(
      deps.registry,
      {
        pendingCalls: deps.pendingCalls,
        completer: deps.completer,
        now: deps.now,
      },
      input.sessionId,
      input.toolCallId,
      outcome.result,
    );
  } catch (err) {
    if (!alreadyCompleted(err)) throw err;
    await deps.pendingCalls.markSubmitted(input.sessionId, input.toolCallId, deps.now());
  }
}

const productionPendingCalls: PendingToolCallStore = {
  recordRequested: (input) => makePendingToolCallStore().recordRequested(input),
  markSubmitted: (sessionId, toolCallId, at) =>
    makePendingToolCallStore().markSubmitted(sessionId, toolCallId, at),
  markCompleted: (sessionId, toolCallId, at) =>
    makePendingToolCallStore().markCompleted(sessionId, toolCallId, at),
  find: (sessionId, toolCallId) =>
    makePendingToolCallStore().find(sessionId, toolCallId),
  listUnsubmittedSessionCallsBefore: (cutoff) =>
    makePendingToolCallStore().listUnsubmittedSessionCallsBefore(cutoff),
};

export const productionToolExecDeps: ToolExecDeps = {
  registry: productionTools,
  resolveContext: resolveToolContext,
  pendingCalls: productionPendingCalls,
  completer: sessions,
  now: () => new Date(),
  nowMs: () => Date.now(),
};
