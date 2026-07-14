import { DBOS } from "@dbos-inc/dbos-sdk";

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
    await deps.pendingCalls.markSubmitted(toolCallId, at);
  } else {
    await deps.pendingCalls.markCompleted(toolCallId, at);
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
        await deps.startWorkflow(input, `toolexec:${requested.toolCallId}`);
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
  });
}
