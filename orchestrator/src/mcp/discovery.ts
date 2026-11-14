import { fromJson } from "@bufbuild/protobuf";
import { z } from "zod";
import type { WorkerIdentity, WorkerIntent } from "../../../mcp-worker/src/contracts.ts";
import type { IntegrationConnectionRow } from "../db/integration-connections.ts";
import { CredentialStatusSchema } from "../gen/engram/mcp/v1/control_pb.ts";
import { assertSchemaShape } from "./catalog.ts";
import { validateMcpConfig } from "./config.ts";
import type { CredentialControlClient } from "./control-client.ts";
import { makeCredentialBroker, type AdmittedCredentialOperation } from "./credential-broker.ts";
import type { CredentialMetadataStore } from "./credential-store.ts";
import { canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import type { makeMcpOperatorRuntime } from "./operator-runtime.ts";
import { makeResourceWorkerClient } from "./worker-client.ts";
import type { McpDiscoveredTool } from "./types.ts";

export interface DiscoveryAdmission {
  reserve(
    input: {
      requestId: string;
      actorId: string;
      connectionId: string;
      configRevision: number;
      authGeneration: number;
      credentialIdentity: string;
      deploymentId: string;
      identity: WorkerIdentity;
      destination: string;
    },
    now: Date,
  ): Promise<Omit<WorkerIntent, "operationDigest">>;
  beginDispatch(requestId: string, intentId: string, now: Date): Promise<void>;
  assertAdmitted(operation: Readonly<AdmittedCredentialOperation>, now: Date): Promise<void>;
  finish(requestId: string, intentId: string, now: Date): Promise<void>;
}
const tool = z
  .object({
    name: z
      .string()
      .min(1)
      .max(256)
      .regex(/^[^\u0000-\u001f]+$/),
    description: z.string().max(16384).optional(),
    inputSchema: z.record(z.string(), z.unknown()),
    outputSchema: z.record(z.string(), z.unknown()).optional(),
    annotations: z.record(z.string(), z.unknown()).optional(),
  })
  .strict();
export function discoveredCatalog(value: unknown): McpDiscoveredTool[] {
  if (Buffer.byteLength(canonicalJson(value)) > MCP_LIMITS.manifestBytes)
    throw Error("MCP catalog exceeds limit");
  const tools = z.array(tool).max(MCP_LIMITS.toolsPerSession).parse(value);
  if (new Set(tools.map((t) => t.name)).size !== tools.length) throw Error("MCP duplicate tool");
  return tools.map((t) => {
    assertSchemaShape(t.inputSchema);
    if (t.outputSchema) assertSchemaShape(t.outputSchema);
    return {
      upstreamName: t.name,
      description: t.description ?? "",
      inputSchema: t.inputSchema,
      ...(t.outputSchema ? { outputSchema: t.outputSchema } : {}),
    };
  });
}
/** Discovery owns one durable administrative intent. No retry path can resend it. */
export function makeMcpDiscovery(deps: {
  admission: DiscoveryAdmission;
  credentials: Pick<CredentialMetadataStore, "get">;
  control: CredentialControlClient;
  operator: ReturnType<typeof makeMcpOperatorRuntime>;
  now(): Date;
}) {
  return {
    async discover(connection: IntegrationConnectionRow, actorId: string) {
      if (connection.provider !== "mcp" || !connection.mcp)
        throw Error("MCP connection unavailable");
      const config = validateMcpConfig(connection.config);
      const selected = await deps.operator.select(config);
      const worker = makeResourceWorkerClient({ tls: selected.tls, policy: selected.policy });
      const requestId = crypto.randomUUID();
      let intent: Omit<WorkerIntent, "operationDigest"> | undefined;
      try {
        const identity = await worker.identity();
        intent = await deps.admission.reserve(
          {
            requestId,
            actorId,
            connectionId: connection.id,
            configRevision: connection.mcp.configRevision,
            authGeneration: connection.mcp.authGeneration,
            credentialIdentity: connection.mcp.credentialIdentity,
            deploymentId: selected.deploymentId,
            identity,
            destination: config.endpoint,
          },
          deps.now(),
        );
        const row = await deps.credentials.get(connection.id);
        if (!row.mcpCredential.status || row.mcpCredential.transition)
          throw Error("MCP credential unavailable");
        const status = fromJson(CredentialStatusSchema, row.mcpCredential.status);
        await deps.admission.beginDispatch(requestId, intent.id, deps.now());
        const broker = makeCredentialBroker({
          control: deps.control,
          worker,
          admission: {
            assertAdmitted: (operation) => deps.admission.assertAdmitted(operation, deps.now()),
          },
          beforeDispatch: () => deps.operator.assertCurrent(config, selected.policy),
          now: () => deps.now().getTime(),
        });
        const response = await broker.dispatch({
          status,
          identity,
          intent,
          operation: { kind: "discover", endpoint: config.endpoint },
        });
        if (!response.ok) throw Error("MCP discovery unavailable");
        await deps.operator.assertCurrent(config, selected.policy);
        return discoveredCatalog(response.result);
      } finally {
        worker.close();
        if (intent) await deps.admission.finish(requestId, intent.id, deps.now());
      }
    },
  };
}
