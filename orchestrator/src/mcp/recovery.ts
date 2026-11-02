import { Code, ConnectError } from "@connectrpc/connect";
import { log } from "../log.ts";
import { and, asc, eq, gt, lt, sql } from "drizzle-orm";
import { sessions } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { mcpExecution } from "../db/schema.ts";
import type { ToolCallCompleter } from "../tools/complete.ts";
import {
  McpRequestDenied,
  type McpRequestDenialCode,
  type makeMcpEffectDriver,
  type McpToolRequest,
} from "./effect-driver.ts";
import type { CuratedEvent } from "../control-plane/session-events.ts";
import { makeMcpJournal } from "./journal.ts";
import { MCP_LIMITS } from "./limits.ts";

/** Only references cross the durable workflow boundary. */
export interface McpExecutionMarker {
  executionId: string;
  sessionId: string;
  runId: string;
  toolCallId: string;
}
/** Durable pointer to the first authenticated coordinator request, not a guest projection. */
export interface McpRequestMarker {
  sessionId: string;
  runId: string;
  toolCallId: string;
  requestEventIdx: string;
  /** Trusted consumer clock plus the fixed maximum preacceptance budget. Never renewed. */
  deadlineMs: number;
}
export type McpRequestDecision =
  | { kind: "accepted"; execution: McpExecutionMarker }
  | { kind: "denied"; code: McpRequestDenialCode };

function marker(call: typeof mcpExecution.$inferSelect): McpExecutionMarker {
  return {
    executionId: call.id,
    sessionId: call.sessionId,
    runId: call.runId,
    toolCallId: call.toolCallId,
  };
}
const active = new Set(["waiting_approval", "authorized", "dispatching"]);
// These exact CompleteToolCall dispositions prove that delivery can no longer occur.
// A host loss, generic precondition failure or transport outage does not prove deletion.
function targetGone(error: unknown): boolean {
  return (
    error instanceof ConnectError &&
    (error.code === Code.NotFound ||
      (error.code === Code.FailedPrecondition &&
        error.metadata.get("engram-error-slug") === "snapshot_invalidated"))
  );
}
type CompletionDisposition = "enqueued" | "target_gone";

/** Plain runtime operations: effects and completion must never run in a DBOS step. */
export function makeMcpRecovery(deps: {
  db?: ReturnType<typeof getDb>;
  driver: ReturnType<typeof makeMcpEffectDriver>;
  deploymentId: string;
  completer?: ToolCallCompleter;
  now(): Date;
  readRequestEvent?: (sessionId: string, index: bigint) => Promise<CuratedEvent | undefined>;
}) {
  const db = deps.db ?? getDb();
  const journal = makeMcpJournal(db);
  async function existing(ref: Pick<McpRequestMarker, "sessionId" | "toolCallId">) {
    return (
      await db
        .select()
        .from(mcpExecution)
        .where(
          and(
            eq(mcpExecution.sessionId, ref.sessionId),
            eq(mcpExecution.toolCallId, ref.toolCallId),
          ),
        )
    )[0];
  }
  return {
    /** The durable step records only this decision. It never runs a provider operation. */
    async decideRequest(ref: McpRequestMarker): Promise<McpRequestDecision> {
      try {
        const first = await existing(ref);
        if (first) return { kind: "accepted", execution: marker(first) };
        if (!Number.isSafeInteger(ref.deadlineMs) || ref.deadlineMs <= deps.now().getTime())
          return { kind: "denied", code: "deadline_expired" };
        if (!/^[1-9][0-9]{0,18}$/.test(ref.requestEventIdx)) throw Error("invalid event reference");
        const index = BigInt(ref.requestEventIdx);
        if (index > 9223372036854775807n) throw Error("invalid event reference");
        const event = deps.readRequestEvent
          ? await deps.readRequestEvent(ref.sessionId, index)
          : (
              await sessions.listSessionEvents({
                sessionId: ref.sessionId,
                afterIdx: index - 1n,
                limit: 1n,
              })
            ).events[0];
        if (!event || event.idx !== index || event.kind !== "tool_call_requested")
          throw Error("request reference unavailable");
        const payload: unknown = JSON.parse(event.payloadJson);
        if (
          !payload ||
          typeof payload !== "object" ||
          !("tool_call_id" in payload) ||
          payload.tool_call_id !== ref.toolCallId ||
          !("run_id" in payload) ||
          payload.run_id !== ref.runId ||
          !("name" in payload) ||
          typeof payload.name !== "string" ||
          !payload.name.startsWith("mcp_") ||
          !("args_json" in payload) ||
          typeof payload.args_json !== "string"
        )
          throw Error("request reference unavailable");
        try {
          const call = await deps.driver.acceptToolRequest(
            {
              sessionId: ref.sessionId,
              runId: ref.runId,
              toolCallId: ref.toolCallId,
              modelToolName: payload.name,
              originalArguments: payload.args_json,
            },
            new Date(ref.deadlineMs),
          );
          return { kind: "accepted", execution: marker(call) };
        } catch (error) {
          // Acceptance may have committed before a later check failed. That original row owns completion.
          const winner = await existing(ref);
          if (winner) return { kind: "accepted", execution: marker(winner) };
          if (error instanceof McpRequestDenied) return { kind: "denied", code: error.code };
          throw error;
        }
      } catch {
        throw Error("MCP request acceptance unavailable");
      }
    },
    async deliverDenial(ref: McpRequestMarker, code: McpRequestDenialCode): Promise<void> {
      let disposition: CompletionDisposition = "enqueued";
      try {
        await (deps.completer ?? sessions).completeToolCall({
          sessionId: ref.sessionId,
          toolCallId: ref.toolCallId,
          resultJson: JSON.stringify({
            source: "runtime",
            denial: code,
            delivery: "available",
            output: {
              isError: true,
              content: [{ type: "text", text: "External tool request denied." }],
            },
          }),
        });
      } catch (error) {
        if (!targetGone(error)) throw Error("MCP denial completion unavailable");
        disposition = "target_gone";
      }
      log.info(
        { sessionId: ref.sessionId, toolCallId: ref.toolCallId, denial: code, disposition },
        "MCP negative completion settled",
      );
    },
    async accept(input: McpToolRequest): Promise<McpExecutionMarker> {
      return marker(await deps.driver.acceptToolRequest(input));
    },
    async expire() {
      await journal.expire(deps.deploymentId, deps.now());
      await journal.expireUnadmitted(deps.now());
    },
    async pending(after?: string) {
      const rows = await db
        .select({
          id: mcpExecution.id,
          sessionId: mcpExecution.sessionId,
          runId: mcpExecution.runId,
          toolCallId: mcpExecution.toolCallId,
          version: mcpExecution.version,
          state: mcpExecution.state,
        })
        .from(mcpExecution)
        .where(
          and(
            lt(mcpExecution.completionEnqueuedVersion, mcpExecution.version),
            after ? gt(mcpExecution.id, after) : undefined,
          ),
        )
        .orderBy(asc(mcpExecution.id))
        .limit(MCP_LIMITS.queueSize);
      return rows.map(({ id, state, version, ...refs }) => ({
        ...refs,
        executionId: id,
        // One live driver per execution; a later committed outcome gets its own delivery marker.
        workflowId: active.has(state) ? `mcp:${id}` : `mcp:${id}:result:${version}`,
      }));
    },
    async runOnce(ref: McpExecutionMarker, signal?: AbortSignal): Promise<"pending" | "done"> {
      let call = await journal.get(ref.executionId);
      if (!call) return "done"; // Existing session retention deletes its journal too.
      if (
        call.sessionId !== ref.sessionId ||
        call.runId !== ref.runId ||
        call.toolCallId !== ref.toolCallId
      )
        throw Error("MCP execution reference changed");
      if (call.state === "waiting_approval" || call.state === "authorized")
        call = await deps.driver.driveOnce(call.id, signal);
      if (active.has(call.state)) return "pending";
      if (call.completionEnqueuedVersion >= call.version) return "done";
      // A scanner can prove interruption, not an upstream outcome. Preserve that distinction.
      const result =
        call.result ??
        (call.state === "outcome_unknown" || call.state === "canceled"
          ? {
              source: "runtime",
              outcome: call.state,
              delivery: "unavailable",
              reason: "execution_interrupted",
            }
          : undefined);
      if (!result) throw Error("MCP committed result unavailable");
      let disposition: CompletionDisposition = "enqueued";
      try {
        await (deps.completer ?? sessions).completeToolCall({
          sessionId: call.sessionId,
          toolCallId: call.toolCallId,
          resultJson: JSON.stringify({ ...result, resultUnavailable: call.resultUnavailable }),
        });
      } catch (error) {
        if (!targetGone(error)) throw Error("MCP completion unavailable");
        disposition = "target_gone";
      }
      // This legacy field records delivery settlement: enqueue ACK OR authoritative target loss.
      // It proves neither upstream outcome nor guest ACK. A newer result remains pending.
      const settled = await db
        .update(mcpExecution)
        .set({ completionEnqueuedVersion: call.version })
        .where(
          and(
            eq(mcpExecution.id, call.id),
            lt(mcpExecution.completionEnqueuedVersion, call.version),
            sql`${mcpExecution.version} >= ${call.version}`,
          ),
        )
        .returning({ id: mcpExecution.id });
      if (settled.length)
        log.info(
          {
            executionId: call.id,
            sessionId: call.sessionId,
            toolCallId: call.toolCallId,
            version: call.version,
            disposition,
          },
          "MCP completion settled",
        );
      return "done";
    },
  };
}
export type McpRecovery = ReturnType<typeof makeMcpRecovery>;

// Task 6 installs trusted operator/host composition before DBOS launch. Absence stays closed.
let runtime: McpRecovery | undefined;
export function installMcpRecovery(value: McpRecovery): void {
  if (runtime) throw Error("MCP runtime already installed");
  runtime = value;
}
export function mcpRecovery(): McpRecovery | undefined {
  return runtime;
}
