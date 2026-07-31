import { DBOS } from "@dbos-inc/dbos-sdk";
import { eq } from "drizzle-orm";

import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";

import type { CuratedEvent } from "../control-plane/session-events.ts";
import { log as rootLog } from "../log.ts";
import { makePendingToolCallStore, type PendingToolCallStore } from "../tools/pending-tool-calls.ts";
import { tools as productionTools, type ToolRegistry } from "../tools/registry.ts";
import type { ToolExecInput } from "../tools/exec.ts";
import { toolExecWorkflow } from "../workflows/tool-exec.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "tool-consumer" });

export type ToolWorkflowStarter = (
  input: ToolExecInput,
  workflowId: string,
) => Promise<void>;

export interface ToolConsumerDeps {
  registry: ToolRegistry;
  pendingCalls: PendingToolCallStore;
  startWorkflow: ToolWorkflowStarter;
  now: () => Date;
  /** ADR 0107 headless policy: true when the session's task exists and has no
   *  human owner (an automation). Absent = never auto-approve. */
  sessionIsOwnerless?: (sessionId: string) => Promise<boolean>;
  /** The internal completer (`tools.complete`) the auto-approve rides. */
  completeSessionTool?: (
    sessionId: string,
    toolCallId: string,
    result: unknown,
  ) => Promise<void>;
}

interface ToolCallRequestedPayload {
  toolCallId: string;
  name: string;
  argsJson?: string;
}

export function parseToolCallRequested(
  event: CuratedEvent,
): ToolCallRequestedPayload | undefined {
  if (event.kind !== "tool_call_requested") return undefined;
  try {
    const payload = JSON.parse(event.payloadJson) as {
      tool_call_id?: unknown;
      name?: unknown;
      args_json?: unknown;
    };
    if (
      typeof payload.tool_call_id !== "string" ||
      typeof payload.name !== "string"
    ) {
      return undefined;
    }
    return {
      toolCallId: payload.tool_call_id,
      name: payload.name,
      ...(typeof payload.args_json === "string"
        ? { argsJson: payload.args_json }
        : {}),
    };
  } catch {
    return undefined;
  }
}

function eventTimestamp(
  payload: Record<string, unknown>,
  now: () => Date,
): Date {
  if (typeof payload.at === "string") {
    const parsed = new Date(payload.at);
    if (!Number.isNaN(parsed.getTime())) return parsed;
  }
  return now();
}

/** Idempotently project one generic tool lifecycle event into the pending-call
 * ledger. Unknown names deliberately default to handled. */
export async function bookkeepToolEvent(
  sessionId: string,
  event: CuratedEvent,
  deps: Pick<ToolConsumerDeps, "registry" | "pendingCalls" | "now">,
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
    await deps.pendingCalls.recordRequested({
      sessionId,
      toolCallId,
      toolName,
      handling: deps.registry.get(toolName)?.handling ?? "handled",
      requestedAt: at,
    });
  } else if (event.kind === "tool_result_submitted") {
    await deps.pendingCalls.markSubmitted(sessionId, toolCallId, at);
  } else {
    await deps.pendingCalls.markCompleted(sessionId, toolCallId, at);
  }
}

export function makeToolConsumer(deps: ToolConsumerDeps): SessionConsumer {
  return {
    name: "tool-dispatch",
    appliesTo: async () => true,
    interestedIn: (kind) =>
      kind === "tool_call_requested" ||
      kind === "tool_result_submitted" ||
      kind === "tool_call_completed",
    async handle(event, ctx) {
      await bookkeepToolEvent(ctx.sessionId, event, deps);
      const requested = parseToolCallRequested(event);
      if (!requested) return;
      const tool = deps.registry.get(requested.name);
      // ADR 0107 headless policy: a plan proposed in an OWNERLESS session
      // (an automation — nobody is watching) auto-approves immediately, so
      // a cron session gets plan-then-implement instead of parking forever.
      // The plan stays in session_events as a durable, reviewable record.
      // Best-effort + idempotent: a redelivered request re-runs the same
      // complete, which the completion guard treats as already-submitted.
      if (
        tool?.handling === "session" &&
        requested.name === "exit_plan_mode" &&
        deps.sessionIsOwnerless &&
        deps.completeSessionTool
      ) {
        try {
          if (await deps.sessionIsOwnerless(ctx.sessionId)) {
            log.info(
              { sessionId: ctx.sessionId, toolCallId: requested.toolCallId },
              "auto-approving exit_plan_mode for an ownerless (automation) task",
            );
            await deps.completeSessionTool(ctx.sessionId, requested.toolCallId, {
              decision: "approve",
            });
          }
        } catch (err) {
          log.error(
            { sessionId: ctx.sessionId, toolCallId: requested.toolCallId, err },
            "exit_plan_mode auto-approve failed; the plan stays parked",
          );
        }
        return;
      }
      if (!tool || tool.handling !== "handled") return;

      const input: ToolExecInput = {
        sessionId: ctx.sessionId,
        toolCallId: requested.toolCallId,
        toolName: requested.name,
        ...(requested.argsJson !== undefined
          ? { argsJson: requested.argsJson }
          : {}),
      };
      try {
        await deps.startWorkflow(
          input,
          `toolexec:${ctx.sessionId}:${requested.toolCallId}`,
        );
      } catch (err) {
        log.error(
          {
            sessionId: ctx.sessionId,
            toolCallId: requested.toolCallId,
            toolName: requested.name,
            err,
          },
          "tool workflow start failure",
        );
        throw err;
      }
    },
  };
}

export function makeProductionToolConsumer(): SessionConsumer {
  const pendingCalls = makePendingToolCallStore();
  return makeToolConsumer({
    registry: productionTools,
    pendingCalls,
    now: () => new Date(),
    startWorkflow: async (input, workflowId) => {
      await DBOS.startWorkflow(toolExecWorkflow, { workflowID: workflowId })(input);
    },
    sessionIsOwnerless: async (sessionId) => {
      const rows = await getDb()
        .select({ createdByUserId: taskTable.createdByUserId })
        .from(taskSessionTable)
        .innerJoin(taskTable, eq(taskSessionTable.taskId, taskTable.id))
        .where(eq(taskSessionTable.sessionId, sessionId))
        .limit(1);
      // Only a REAL task row with no human creator counts — a session with
      // no task at all is not an automation, just unattributed.
      return rows.length > 0 && rows[0]!.createdByUserId == null;
    },
    completeSessionTool: (sessionId, toolCallId, result) =>
      productionTools.complete(sessionId, toolCallId, result),
  });
}
