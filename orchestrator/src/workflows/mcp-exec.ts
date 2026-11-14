import { DBOS } from "@dbos-inc/dbos-sdk";
import {
  mcpRecovery,
  type McpExecutionMarker,
  type McpRequestMarker,
  type McpRequestDecision,
} from "../mcp/recovery.ts";

async function mcpExecWorkflowEntry(marker: McpExecutionMarker): Promise<void> {
  for (;;) {
    try {
      const runtime = mcpRecovery();
      // Plain calls deliberately run outside DBOS steps: only the journal authorizes effects.
      if (runtime && (await runtime.runOnce(marker)) === "done") return;
    } catch {
      // All errors stay transient and private. Original journal deadlines are never renewed.
    }
    await DBOS.sleep(1_000);
  }
}
export const mcpExecWorkflow = DBOS.registerWorkflow(mcpExecWorkflowEntry, {
  name: "McpExecWorkflow",
});
export async function startMcpWorkflow(
  marker: McpExecutionMarker,
  workflowId = `mcp:${marker.executionId}`,
): Promise<void> {
  await DBOS.startWorkflow(mcpExecWorkflow, { workflowID: workflowId })(marker);
}

/** The first decision is durable before either delivery or child execution starts. */
async function mcpRequestWorkflowEntry(ref: McpRequestMarker): Promise<void> {
  let decision: McpRequestDecision;
  for (;;) {
    try {
      decision = await DBOS.runStep(
        async () => {
          const runtime = mcpRecovery();
          if (!runtime) throw Error("MCP request acceptance unavailable");
          return runtime.decideRequest(ref);
        },
        { name: "mcp-request-decision", retriesAllowed: false },
      );
      break;
    } catch {
      await DBOS.sleep(1_000);
    }
  }
  if (decision.kind === "accepted") {
    await startMcpWorkflow(decision.execution);
    return;
  }
  for (;;) {
    try {
      const runtime = mcpRecovery();
      if (runtime) {
        await runtime.deliverDenial(ref, decision.code);
        return;
      }
    } catch {
      /* Retry only the fixed negative completion, never acceptance. */
    }
    await DBOS.sleep(1_000);
  }
}
export const mcpRequestWorkflow = DBOS.registerWorkflow(mcpRequestWorkflowEntry, {
  name: "McpRequestWorkflow",
});
export async function startMcpRequestWorkflow(ref: McpRequestMarker): Promise<void> {
  await DBOS.startWorkflow(mcpRequestWorkflow, {
    workflowID: `mcp-request:${ref.sessionId}:${ref.toolCallId}`,
  })(ref);
}
