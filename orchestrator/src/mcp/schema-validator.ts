import { canonical } from "../../../mcp-worker/src/index.ts";
import { digest } from "../../../mcp-worker/src/index.ts";
import type { WorkerIntent, WorkerOperation } from "../../../mcp-worker/src/contracts.ts";
import type { IntegrationConnectionRow } from "../db/integration-connections.ts";
import { validateMcpConfig } from "./config.ts";
import type { makeMcpDiscoveryAdmission } from "./discovery-admission.ts";
import type { makeMcpOperatorRuntime } from "./operator-runtime.ts";
import type { McpReviewedTool } from "./types.ts";
import { makeResourceWorkerClient } from "./worker-client.ts";

/** Compilation runs in the isolated worker's disposable schema child. The O process
 * sends no credentials and never starts a schema process beside application secrets. */
export function makeMcpSchemaValidator(deps: {
  admission: Pick<
    ReturnType<typeof makeMcpDiscoveryAdmission>,
    "reserveSchema" | "beginDispatch" | "assertSchemaAdmitted" | "finish"
  >;
  operator: ReturnType<typeof makeMcpOperatorRuntime>;
  now(): Date;
}) {
  return async function validate(
    tools: readonly McpReviewedTool[],
    connection: IntegrationConnectionRow,
    actorId: string,
  ) {
    if (connection.provider !== "mcp" || !connection.mcp) throw Error("MCP connection unavailable");
    const config = validateMcpConfig(connection.config),
      selected = await deps.operator.select(config);
    const operation: Extract<WorkerOperation, { kind: "validateSchemas" }> = {
      kind: "validateSchemas",
      documents: tools.flatMap((t) =>
        t.outputSchema ? [t.inputSchema, t.outputSchema] : [t.inputSchema],
      ),
    };
    const worker = makeResourceWorkerClient({ tls: selected.tls, policy: selected.policy });
    const requestId = crypto.randomUUID();
    let intent: Omit<WorkerIntent, "operationDigest"> | undefined;
    try {
      const identity = await worker.identity();
      intent = await deps.admission.reserveSchema(
        {
          requestId,
          actorId,
          connectionId: connection.id,
          configRevision: connection.mcp.configRevision,
          authGeneration: connection.mcp.authGeneration,
          credentialIdentity: connection.mcp.credentialIdentity,
          deploymentId: selected.deploymentId,
          identity,
          documents: operation.documents,
        },
        deps.now(),
      );
      await deps.admission.beginDispatch(requestId, intent.id, deps.now());
      if (canonical(await worker.identity()) !== canonical(identity))
        throw Error("MCP schema worker changed");
      await deps.operator.assertCurrent(config, selected.policy);
      await deps.admission.assertSchemaAdmitted(
        { identity, intent, documents: operation.documents },
        deps.now(),
      );
      const response = await worker.dispatch({
        protocol: 1,
        intent: { ...intent, operationDigest: digest(operation) },
        operation,
      });
      if (!response.ok || response.result !== true) throw Error("MCP schema validation failed");
      await deps.operator.assertCurrent(config, selected.policy);
    } finally {
      worker.close();
      if (intent) await deps.admission.finish(requestId, intent.id, deps.now());
    }
  };
}
