/**
 * External CompleteToolCall preflight (ADR 0089 §8).
 *
 * Ownership is enforced by the generic passthrough gate before this guard.
 * This second gate binds the request to a durable pending call and permits only
 * `handling:"session"`; orchestrator-handled calls can be completed only by
 * internal code.
 */

import { Code, ConnectError, type HandlerContext } from "@connectrpc/connect";

import {
  makePendingToolCallStore,
  type PendingToolCallStore,
} from "../tools/pending-tool-calls.ts";
import { validateToolResult } from "../tools/complete.ts";
import { tools as productionTools, type ToolRegistry } from "../tools/registry.ts";

export function makeExternalToolCompletionGuard(deps?: {
  pendingCalls?: PendingToolCallStore;
  registry?: ToolRegistry;
}): (req: unknown, ctx: HandlerContext) => Promise<void> {
  return async (req: unknown) => {
    const request = req as {
      sessionId?: unknown;
      toolCallId?: unknown;
      resultJson?: unknown;
    };
    if (typeof request.sessionId !== "string" || typeof request.toolCallId !== "string") {
      throw new ConnectError("tool call not found", Code.NotFound);
    }

    const pendingCalls = deps?.pendingCalls ?? makePendingToolCallStore();
    const row = await pendingCalls.find(request.toolCallId);
    if (!row || row.sessionId !== request.sessionId) {
      throw new ConnectError("tool call not found", Code.NotFound);
    }
    if (row.handling !== "session") {
      throw new ConnectError(
        "orchestrator-handled tool calls cannot be completed externally",
        Code.PermissionDenied,
      );
    }

    const registry = deps?.registry ?? productionTools;
    const tool = registry.get(row.toolName);
    if (!tool) {
      throw new ConnectError(`tool is not registered: ${row.toolName}`, Code.FailedPrecondition);
    }
    let result: unknown;
    try {
      result = typeof request.resultJson === "string"
        ? JSON.parse(request.resultJson)
        : undefined;
    } catch {
      throw new ConnectError(`invalid result for tool ${row.toolName}`, Code.InvalidArgument);
    }
    try {
      validateToolResult(tool, result);
    } catch {
      throw new ConnectError(`invalid result for tool ${row.toolName}`, Code.InvalidArgument);
    }
  };
}
