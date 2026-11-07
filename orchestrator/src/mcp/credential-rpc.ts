import { Code, ConnectError, type HandlerContext, type ServiceImpl } from "@connectrpc/connect";
import { fromJson } from "@bufbuild/protobuf";
import type { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import {
  CredentialKind,
  CredentialStatusSchema,
  type CredentialStatus,
} from "../gen/engram/mcp/v1/control_pb.ts";
import { requireAdmin, type GetSession } from "../rpc/require.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  mcpOperation,
  requireMcp,
  requireMcpHook,
  requireMcpServices,
  type McpServices,
} from "./contracts.ts";

export async function requireMcpHumanAdmin(ctx: HandlerContext, getSession: GetSession) {
  const actor = await requireAdmin(ctx, getSession);
  if (actor.serviceAccount)
    throw new ConnectError("a human administrator is required", Code.PermissionDenied);
  return actor;
}
function statusToProto(status: CredentialStatus | undefined, revoked: boolean) {
  return {
    status: revoked ? "disconnected" : (status?.status ?? "not_configured"),
    reference: status?.reference?.name || status?.reference?.registrationId || "",
    kind:
      status?.reference?.kind === CredentialKind.API_KEY
        ? "api_key"
        : status?.reference?.kind === CredentialKind.OAUTH
          ? "oauth"
          : "",
    credentialIdentity: status?.context?.credentialIdentity ?? "",
    version: status?.version ?? 0n,
    reservationGeneration: status?.reference?.generation ?? 0n,
  };
}
type Methods =
  | "putMcpCredential"
  | "getMcpSetup"
  | "completeMcpAuthorization"
  | "recoverMcpCredential"
  | "authorizeMcp"
  | "disconnectMcp";
export function credentialHandlers(deps: {
  services?: McpServices;
  connections: IntegrationConnectionStore;
  getSession: GetSession;
  connectionToProto: (row: IntegrationConnectionRow) => { id: string };
}): Pick<ServiceImpl<typeof IntegrationService>, Methods> {
  function admin() {
    return requireMcpHook(requireMcp(deps.services).credentials);
  }
  function recoveryAdmin() {
    return requireMcpHook(requireMcpServices(deps.services).credentials);
  }
  async function result(id: string) {
    const row = await deps.connections.get(id);
    if (!row || row.provider !== "mcp")
      throw new ConnectError("MCP connection not found", Code.NotFound);
    const { status } = await recoveryAdmin().status(id);
    return {
      connection: deps.connectionToProto(row),
      credential: statusToProto(status, row.mcp?.revoked ?? true),
    };
  }
  return {
    async putMcpCredential(req, ctx) {
      const actor = await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = admin();
      const kind =
        req.kind === "api_key"
          ? CredentialKind.API_KEY
          : req.kind === "client_secret"
            ? CredentialKind.CLIENT_SECRET
            : CredentialKind.UNSPECIFIED;
      return mcpOperation(async () => {
        await service.putSecret(
          req.connectionId,
          req.expectedRevision,
          req.expectedGeneration,
          actor.id,
          kind,
          req.header,
          req.value,
        );
        return result(req.connectionId);
      });
    },
    async authorizeMcp(req, ctx) {
      const actor = await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = admin();
      return mcpOperation(() =>
        service.begin(
          req.connectionId,
          req.expectedRevision,
          req.expectedGeneration,
          actor.id,
          req.shareWithOrgSessions,
        ),
      );
    },
    async completeMcpAuthorization(req, ctx) {
      const actor = await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = admin();
      return mcpOperation(async () => {
        await service.complete(
          req.connectionId,
          req.expectedRevision,
          req.expectedGeneration,
          actor.id,
          {
            flowId: req.flowId,
            state: req.state,
            code: req.code,
            issuer: req.issuer,
            redirectUri: req.redirectUri,
          },
        );
        return result(req.connectionId);
      });
    },
    async disconnectMcp(req, ctx) {
      const actor = await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = recoveryAdmin();
      return mcpOperation(async () => {
        await service.disconnect(
          req.connectionId,
          req.expectedRevision,
          req.expectedGeneration,
          actor.id,
        );
        return result(req.connectionId);
      });
    },
    async recoverMcpCredential(req, ctx) {
      const actor = await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = recoveryAdmin();
      return mcpOperation(async () => {
        await service.recover(
          req.connectionId,
          req.expectedRevision,
          req.expectedGeneration,
          actor.id,
        );
        return result(req.connectionId);
      });
    },
    async getMcpSetup(req, ctx) {
      await requireMcpHumanAdmin(ctx, deps.getSession);
      const service = deps.services;
      return mcpOperation(async () => {
        const base = {
          enabled: service?.enabled ?? false,
          recoveryAvailable: !!service?.credentials,
          discoveryAvailable: !!service?.enabled && !!service.discovery,
          registrations: service?.credentials?.registrations() ?? [],
          destinations: service?.destinations ?? [],
        };
        if (!req.connectionId) return base;
        const credentials = requireMcpHook(service?.credentials),
          { row, status } = await credentials.status(req.connectionId);
        const connection = await deps.connections.get(row.id);
        if (!connection) throw new Error("connection not found");
        const client = row.mcpCredential.clientSecret
          ? fromJson(CredentialStatusSchema, row.mcpCredential.clientSecret)
          : undefined;
        return {
          ...base,
          current: {
            connection: deps.connectionToProto(connection),
            credential: statusToProto(status, row.revoked),
          },
          transitionPending: !!row.mcpCredential.transition,
          flowId: row.mcpCredential.flow?.id ?? "",
          flowExpiresAt: row.mcpCredential.flow
            ? new Date(row.mcpCredential.flow.expiresAt).toISOString()
            : "",
          clientSecretConfigured: client?.status === "connected",
        };
      });
    },
  };
}
