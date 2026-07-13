/** Shared result-validation and completion path for ADR 0089 tools. */

import type { PendingToolCallStore } from "./pending-tool-calls.ts";
import type { RegisteredTool, ToolRegistry } from "./registry.ts";

export interface ToolCallCompleter {
  completeToolCall(request: {
    sessionId: string;
    toolCallId: string;
    resultJson: string;
  }): Promise<unknown>;
}

export interface ToolCompletionDeps {
  pendingCalls: PendingToolCallStore;
  completer: ToolCallCompleter;
  now?: () => Date;
}

function isProtocolErrorResult(result: unknown): result is { error: string } {
  if (typeof result !== "object" || result === null || Array.isArray(result)) return false;
  const record = result as Record<string, unknown>;
  return typeof record.error === "string" && Object.keys(record).length === 1;
}

/**
 * Validate and serialize a result before crossing the coordinator seam.
 * `{"error":"<message>"}` is the sole protocol-level exception to a tool's
 * output schema; dispatch uses it for denied/invalid/failed calls.
 */
export function validateToolResult(tool: RegisteredTool, result: unknown): unknown {
  if (isProtocolErrorResult(result)) return result;
  const parsed = tool.output.safeParse(result);
  if (!parsed.success) throw new Error(`invalid result for tool ${tool.name}`);
  return parsed.data;
}

/** Validate a known tool's result before invoking the injected completer. */
export async function completeToolResult(
  tool: RegisteredTool,
  completer: ToolCallCompleter,
  request: { sessionId: string; toolCallId: string },
  result: unknown,
): Promise<string> {
  const validated = validateToolResult(tool, result);
  const resultJson = JSON.stringify(validated);
  await completer.completeToolCall({ ...request, resultJson });
  return resultJson;
}

/** Implementation behind `tools.complete(sessionId, callId, result)`. */
export async function completeRegisteredToolCall(
  registry: ToolRegistry,
  deps: ToolCompletionDeps,
  sessionId: string,
  toolCallId: string,
  result: unknown,
): Promise<void> {
  const pending = await deps.pendingCalls.find(toolCallId);
  if (!pending || pending.sessionId !== sessionId) {
    throw new Error(`pending tool call not found: ${toolCallId}`);
  }
  const tool = registry.get(pending.toolName);
  if (!tool) throw new Error(`tool is not registered: ${pending.toolName}`);

  await completeToolResult(tool, deps.completer, { sessionId, toolCallId }, result);
  await deps.pendingCalls.markSubmitted(toolCallId, (deps.now ?? (() => new Date()))());
}
