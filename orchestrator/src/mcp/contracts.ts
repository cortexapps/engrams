import { sessionMcpState, type McpReviewProducers } from "./review-state.ts";
import { create, toBinary, toJson } from "@bufbuild/protobuf";
import {
  McpApprovalReviewContextSchema,
  McpCompletionSettlement,
  GetMcpExecutionResultResponseSchema,
  McpRebindDrainSchema,
  McpRebindDrainReason,
} from "../gen/engram/app/v1/integration_pb.ts";
import { McpPublicationRequired, McpRebindDraining, type McpRebindDrain } from "./rebind.ts";
import { policyReviewCurrent, type JournalReviewContext } from "./journal-review.ts";
import { credentialHandlers, requireMcpHumanAdmin } from "./credential-rpc.ts";
import type { CredentialAdmin } from "./credential-admin.ts";
import { Code, ConnectError, type ServiceImpl } from "@connectrpc/connect";
import type { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { requireAdmin, requireUser, type GetSession } from "../rpc/require.ts";
import { validateMcpConfig, type McpConfig } from "./config.ts";
import type { McpRepository } from "./repository.ts";
import type { McpJournal, McpUnknownContext } from "./journal.ts";
import type { McpDiscoveredTool, McpReviewedTool } from "./types.ts";
import { assertSchemaShape } from "./catalog.ts";
import { MCP_LIMITS } from "./limits.ts";
import { canonicalJson } from "./hash.ts";
import { verifiedExecutionResult } from "./effect-driver.ts";

/** Internal services use trusted connection identities, never guest destinations. */
export interface McpServices {
  enabled: boolean;
  repository: McpRepository;
  journal: McpJournal;
  reviewProducers?: McpReviewProducers;
  sessionOwner?: (sessionId: string) => Promise<string | null>;
  validateDestination?: (config: McpConfig) => Promise<void>;
  validateSchemas?: (tools: readonly McpReviewedTool[]) => Promise<void>;
  discovery?: { discover(connection: IntegrationConnectionRow): Promise<McpDiscoveredTool[]> };
  credentials?: CredentialAdmin;
  destinations?: { zoneId: string; endpoint: string }[];
  runtime?: {
    cancel(executionId: string): Promise<void>;
    /** Both operations authorize a fresh owner/admin; commit requires the reviewed candidate. */
    prepareRebind(
      sessionId: string,
      expectedBindingHash: string,
      actorId: string,
    ): Promise<
      import("./session-bindings.ts").McpSessionBindingSnapshot & { drain?: McpRebindDrain }
    >;
    rebind(
      sessionId: string,
      expectedBindingHash: string,
      expectedCandidateHash: string,
      actorId: string,
    ): Promise<{ bindingHash: string }>;
  };
}
export function requireMcp(services: McpServices | undefined): McpServices {
  if (!services?.enabled)
    throw new ConnectError("external MCP is disabled", Code.FailedPrecondition);
  return services;
}
export function requireMcpHook<T>(hook: T | undefined): T {
  if (!hook)
    throw new ConnectError("external MCP service is not configured", Code.FailedPrecondition);
  return hook;
}
function drainProto(drain: McpRebindDrain) {
  const reason =
    drain.reason === "dispatch_in_progress"
      ? McpRebindDrainReason.DISPATCH_IN_PROGRESS
      : drain.reason === "unknown_intent"
        ? McpRebindDrainReason.UNKNOWN_INTENT
        : McpRebindDrainReason.LEGACY_INTENT;
  return { reason, retryAfter: drain.retryAfter };
}
export async function mcpOperation<T>(run: () => Promise<T>): Promise<T> {
  try {
    return await run();
  } catch (error) {
    if (error instanceof ConnectError) throw error;
    if (error instanceof McpPublicationRequired)
      throw new ConnectError(
        "An administrator must publish MCP grants before rebind",
        Code.FailedPrecondition,
      );
    if (error instanceof McpRebindDraining)
      throw new ConnectError("MCP calls are still draining", Code.FailedPrecondition, undefined, [
        { desc: McpRebindDrainSchema, value: drainProto(error.drain) },
      ]);
    const message = error instanceof Error ? error.message : "MCP operation failed";
    const code = /owner or administrator/.test(message)
      ? Code.PermissionDenied
      : /stale|already/.test(message)
        ? Code.Aborted
        : Code.FailedPrecondition;
    // Internal worker errors can contain upstream metadata. Return a stable error.
    throw new ConnectError(
      code === Code.PermissionDenied
        ? "session owner or administrator required"
        : code === Code.Aborted
          ? "MCP state changed; reload and try again"
          : "MCP operation could not be completed",
      code,
    );
  }
}
export interface McpProtoConfig {
  endpoint: string;
  zone: string;
  authKind: string;
  apiKeySecretRef: string;
  oauthClientRegistrationId: string;
  shareWithOrgSessions: boolean;
}
export function mcpConfigFromProto(value: McpProtoConfig | undefined): McpConfig {
  if (!value) throw new ConnectError("MCP config is required", Code.InvalidArgument);
  if (
    (value.authKind === "api_key" &&
      (value.oauthClientRegistrationId || value.shareWithOrgSessions)) ||
    (value.authKind === "oauth" && value.apiKeySecretRef)
  )
    throw new ConnectError("conflicting MCP credential configuration", Code.InvalidArgument);
  try {
    return validateMcpConfig({
      endpoint: value.endpoint,
      zone: value.zone,
      auth:
        value.authKind === "api_key"
          ? { kind: value.authKind, secretRef: value.apiKeySecretRef }
          : {
              kind: value.authKind,
              clientRegistrationId: value.oauthClientRegistrationId,
              shareWithOrgSessions: value.shareWithOrgSessions,
            },
    });
  } catch {
    throw new ConnectError("invalid MCP configuration", Code.InvalidArgument);
  }
}
export function mcpConfigToProto(raw: unknown): McpProtoConfig {
  const value = validateMcpConfig(raw);
  return {
    endpoint: value.endpoint,
    zone: value.zone,
    authKind: value.auth.kind,
    apiKeySecretRef: value.auth.kind === "api_key" ? value.auth.secretRef : "",
    oauthClientRegistrationId: value.auth.kind === "oauth" ? value.auth.clientRegistrationId : "",
    shareWithOrgSessions: value.auth.kind === "oauth" && value.auth.shareWithOrgSessions,
  };
}
function catalogToProto(value: Awaited<ReturnType<McpRepository["saveDraft"]>>) {
  if (
    !Array.isArray(value.tools) ||
    value.tools.length > MCP_LIMITS.toolsPerSession ||
    Buffer.byteLength(JSON.stringify(value.tools)) > MCP_LIMITS.manifestBytes
  )
    throw new Error("catalog is too large");
  for (const tool of value.tools) {
    if (typeof tool.upstreamName !== "string" || typeof tool.description !== "string")
      throw new Error("invalid catalog");
    assertSchemaShape(tool.inputSchema);
    if (tool.outputSchema !== undefined) assertSchemaShape(tool.outputSchema);
  }
  return {
    id: value.id,
    connectionId: value.connectionId,
    kind: value.kind,
    revision: value.revision,
    configRevision: value.configRevision,
    authGeneration: value.authGeneration,
    fingerprint: value.fingerprint,
    tools: value.tools.map((t) => ({
      upstreamName: t.upstreamName,
      description: t.description,
      inputSchemaJson: JSON.stringify(t.inputSchema),
      outputSchemaJson: t.outputSchema ? JSON.stringify(t.outputSchema) : "",
      classification: "classification" in t ? String(t.classification) : "",
      enabled: "enabled" in t && t.enabled === true,
      fingerprint: "fingerprint" in t ? String(t.fingerprint) : "",
    })),
  };
}
function parseSchema(value: string): Record<string, unknown> {
  try {
    const schema: unknown = JSON.parse(value);
    if (!schema || typeof schema !== "object" || Array.isArray(schema)) throw new Error();
    return schema as Record<string, unknown>;
  } catch {
    throw new ConnectError("schema must be a JSON object", Code.InvalidArgument);
  }
}
type Methods =
  | "putMcpCredential"
  | "getMcpSetup"
  | "completeMcpAuthorization"
  | "recoverMcpCredential"
  | "getMcpCatalogs"
  | "discoverMcp"
  | "publishMcp"
  | "authorizeMcp"
  | "disconnectMcp"
  | "getSessionMcpState"
  | "getMcpExecutionResult"
  | "listMcpApprovals"
  | "decideMcpApproval"
  | "cancelMcpCall"
  | "resolveMcpOutcome"
  | "prepareMcpRebind"
  | "rebindMcp";
export function mcpHandlers(deps: {
  services?: McpServices;
  connections: IntegrationConnectionStore;
  getSession: GetSession;
  now: () => Date;
  connectionToProto: (row: IntegrationConnectionRow) => { id: string };
}): Pick<ServiceImpl<typeof IntegrationService>, Methods> {
  const services = () => requireMcp(deps.services);
  async function connection(id: string) {
    const row = await deps.connections.get(id);
    if (!row || row.provider !== "mcp" || !row.mcp)
      throw new ConnectError("MCP connection not found", Code.NotFound);
    return row;
  }
  return {
    ...credentialHandlers(deps),
    async getMcpCatalogs(req, ctx) {
      await requireMcpHumanAdmin(ctx, deps.getSession);
      const repository = requireMcpHook(deps.services?.repository);
      return mcpOperation(async () => {
        const row = await connection(req.connectionId);
        const [draft, published] = await Promise.all([
          repository.getLatestDraft(row.id),
          row.mcp!.catalogRevision ? repository.getCatalog(row.id, row.mcp!.catalogRevision) : null,
        ]);
        const result = {
          configRevision: row.mcp!.configRevision,
          catalogRevision: row.mcp!.catalogRevision,
          ...(draft ? { draft: catalogToProto(draft) } : {}),
          ...(published ? { published: catalogToProto(published) } : {}),
        };
        if (Buffer.byteLength(JSON.stringify(result)) > MCP_LIMITS.responseBytes)
          throw new Error("catalog response is too large");
        return result;
      });
    },
    async discoverMcp(req, ctx) {
      const actor = await requireAdmin(ctx, deps.getSession);
      const service = services();
      const discover = requireMcpHook(service.discovery);
      return mcpOperation(async () => {
        const row = await connection(req.connectionId);
        if (row.mcp!.configRevision !== req.expectedRevision) throw new Error("stale connection");
        if (row.mcp!.revoked || !row.mcp!.credentialIdentity)
          throw new Error("credential setup required");
        const tools = await discover.discover(row);
        return {
          catalog: catalogToProto(
            await service.repository.saveDraft(row.id, row.mcp!, tools, actor.id, deps.now()),
          ),
        };
      });
    },
    async publishMcp(req, ctx) {
      const actor = await requireAdmin(ctx, deps.getSession);
      const service = services();
      const validate = requireMcpHook(service.validateSchemas);
      const { reviewCatalog } = await import("./catalog.ts");
      let tools: McpReviewedTool[];
      try {
        tools = reviewCatalog(
          req.tools.map((t) => ({
            upstreamName: t.upstreamName,
            description: t.description,
            inputSchema: parseSchema(t.inputSchemaJson),
            ...(t.outputSchemaJson ? { outputSchema: parseSchema(t.outputSchemaJson) } : {}),
            classification: t.classification,
            enabled: t.enabled,
          })),
        );
      } catch {
        throw new ConnectError("invalid reviewed tools", Code.InvalidArgument);
      }
      return mcpOperation(async () => {
        await validate(tools);
        return {
          catalog: catalogToProto(
            await service.repository.publish(
              req.connectionId,
              req.draftId,
              req.expectedRevision,
              tools,
              actor.id,
              deps.now(),
            ),
          ),
        };
      });
    },
    async getMcpExecutionResult(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      return mcpOperation(async () => {
        const call = await service.journal.readExecutionResult(
          req.sessionId,
          req.executionId,
          actor.id,
        );
        let resultJson: string | undefined;
        if (call.result !== null) {
          resultJson = canonicalJson(call.result);
          if (Buffer.byteLength(resultJson) > MCP_LIMITS.resultBytes)
            throw new Error("result exceeds limit");
          if (call.result.source === "local_validation") {
            const expected = {
              source: "local_validation",
              delivery: "available",
              output: {
                content: [
                  { type: "text", text: "Tool arguments do not satisfy the approved schemas." },
                ],
                isError: true,
              },
            };
            if (
              resultJson !== canonicalJson(expected) ||
              call.state !== "failed" ||
              call.resultUnavailable
            )
              throw new Error("invalid local result");
          } else {
            const verified = verifiedExecutionResult(call.result);
            if (
              verified.state !== call.state ||
              verified.resultUnavailable !== call.resultUnavailable
            )
              throw new Error("invalid result state");
          }
        }
        const terminal = ["succeeded", "failed", "canceled", "outcome_unknown"].includes(
          call.state,
        );
        const response = {
          state: call.state,
          resultUnavailable: call.resultUnavailable,
          resultJson,
          settlement: !terminal
            ? McpCompletionSettlement.NOT_APPLICABLE
            : call.completionEnqueuedVersion >= call.version
              ? McpCompletionSettlement.SETTLED_OR_TARGET_GONE
              : McpCompletionSettlement.PENDING,
        };
        const message = create(GetMcpExecutionResultResponseSchema, response);
        const size = Math.max(
          toBinary(GetMcpExecutionResultResponseSchema, message).byteLength,
          Buffer.byteLength(JSON.stringify(toJson(GetMcpExecutionResultResponseSchema, message))),
        );
        if (size > MCP_LIMITS.responseBytes) throw new Error("result response exceeds limit");
        return response;
      });
    },
    async getSessionMcpState(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      return mcpOperation(() =>
        sessionMcpState({
          journal: service.journal,
          sessionId: req.sessionId,
          actorId: actor.id,
          now: deps.now(),
          after: req.afterExecutionId || undefined,
          limit: req.pageSize || undefined,
          planToolCallId: req.planToolCallId,
          producers: service.reviewProducers,
          canCancel: Boolean(service.runtime?.cancel),
          canRebind: Boolean(service.runtime?.prepareRebind && service.runtime?.rebind),
          afterBindingToolName: req.afterBindingToolName || undefined,
          bindingPageSize: req.bindingPageSize || undefined,
          omitBindings: req.omitBindings,
          omitExecutions: req.omitExecutions,
        }),
      );
    },
    async listMcpApprovals(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      // Use the same bounded read; a continuation is available from GetSessionMcpState.
      return mcpOperation(async () => {
        const state = await sessionMcpState({
          journal: service.journal,
          sessionId: req.sessionId,
          actorId: actor.id,
          now: deps.now(),
          after: req.afterExecutionId || undefined,
          producers: service.reviewProducers,
          canCancel: Boolean(service.runtime?.cancel),
          omitBindings: true,
        });
        return { approvals: state.approvals, nextExecutionId: state.nextExecutionId };
      });
    },
    async decideMcpApproval(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      if (!req.expectedReviewContext)
        throw new ConnectError("exact review context is required", Code.InvalidArgument);
      // Copy known fields only. Protobuf runtime metadata is not review identity.
      const expected = toJson(McpApprovalReviewContextSchema, req.expectedReviewContext, {
        alwaysEmitImplicit: true,
      }) as JournalReviewContext;
      return mcpOperation(async () => {
        await service.journal.readSession(expected.sessionId, actor.id, { limit: 1 });
        const producers = requireMcpHook(service.reviewProducers);
        const current = await producers.bindings.readCurrent(expected.sessionId);
        const policy = await requireMcpHook(producers.policy.readWritePolicy)({
          protocol: 1,
          sessionId: expected.sessionId,
        });
        if (
          current.bindingHash !== expected.bindingHash ||
          !policy.state?.policy?.ref ||
          policy.state.policy.ref.policyRevision?.toString() !== expected.policyRevision ||
          policy.state.policy.ref.bindingEpoch?.toString() !== expected.bindingEpoch
        )
          throw new Error("stale review policy");
        const call = await service.journal.get(req.executionId);
        if (!call?.requestContext || !policyReviewCurrent(call.requestContext, policy.state.policy))
          throw new Error("stale review policy lineage");
        if (
          req.approve &&
          (!policy.state.policy.current ||
            !policy.state.policy.initialized ||
            policy.state.policy.writeEligibility !== "write_eligible")
        )
          throw new Error("current policy denies writes");
        const result = await service.journal.decide(
          req.executionId,
          actor.id,
          req.approve,
          expected,
          deps.now(),
        );
        return {
          state: result.call.state,
          receipt: {
            executionId: result.call.id,
            decision: result.approval.decision,
            actorId: result.approval.actorId!,
            decidedAt: result.approval.decidedAt!.toISOString(),
            journalRevision: result.call.version,
            reviewedContext: result.reviewedContext,
          },
        };
      });
    },
    async cancelMcpCall(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      const runtime = requireMcpHook(service.runtime);
      return mcpOperation(async () => {
        if (!req.expectedJournalRevision)
          throw new ConnectError("expected journal revision is required", Code.InvalidArgument);
        const call = await service.journal.cancel(
          req.executionId,
          actor.id,
          deps.now(),
          req.expectedJournalRevision,
        );
        // The committed cancellation fences other replicas even if local I/O already ended.
        await runtime.cancel(call.id);
        const receipt = await service.journal.cancellationReceipt(call.id);
        if (!receipt) throw Error("cancellation receipt unavailable");
        return {
          state: call.state,
          receipt: {
            ...receipt,
            actorId: receipt.actorId ?? "",
            createdAt: receipt.createdAt.toISOString(),
          },
        };
      });
    },
    async resolveMcpOutcome(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const service = services();
      if (actor.serviceAccount)
        throw new ConnectError("a human decision is required", Code.PermissionDenied);
      if (req.outcome !== "succeeded" && req.outcome !== "failed" && req.outcome !== "acknowledged")
        throw new ConnectError("invalid outcome resolution", Code.InvalidArgument);
      const outcome = req.outcome;
      if (!req.expectedUnknownContext)
        throw new ConnectError("exact unknown context is required", Code.InvalidArgument);
      const context = req.expectedUnknownContext;
      const expected: McpUnknownContext = {
        executionId: context.executionId,
        journalRevision: context.journalRevision,
        requestDigest: context.requestDigest,
        bindingHash: context.bindingHash,
        intentId: context.intentId,
      };
      return mcpOperation(async () => {
        const receipt = await service.journal.resolveUnknown(
          req.executionId,
          actor.id,
          outcome,
          deps.now(),
          expected,
        );
        return {
          outcome: receipt.outcome,
          receipt: {
            ...receipt,
            createdAt: receipt.createdAt.toISOString(),
            expectedContext: receipt.expectedContext ?? undefined,
          },
        };
      });
    },
    async prepareMcpRebind(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      if (actor.serviceAccount)
        throw new ConnectError("a human decision is required", Code.PermissionDenied);
      const runtime = requireMcpHook(services().runtime);
      if (!req.expectedBindingHash)
        throw new ConnectError("expected binding hash is required", Code.InvalidArgument);
      return mcpOperation(async () => {
        const snapshot = await runtime.prepareRebind(
          req.sessionId,
          req.expectedBindingHash,
          actor.id,
        );
        const bindingTools = snapshot.immutableBindings
          .flatMap((binding) =>
            binding.tools.map((bound) => ({
              connectionId: binding.connectionId,
              connectionAlias: binding.alias,
              modelToolName: bound.name,
              reviewedTool: {
                ...bound.tool,
                inputSchemaJson: JSON.stringify(bound.tool.inputSchema),
                outputSchemaJson: bound.tool.outputSchema
                  ? JSON.stringify(bound.tool.outputSchema)
                  : "",
              },
              argumentConstraintDocumentsJson: bound.grant.argumentConstraintDocuments.map(
                (document) => JSON.stringify(document),
              ),
              automationWrite: bound.grant.automationWrite,
              unrestricted: bound.grant.unrestricted,
              catalogId: binding.catalogId,
              catalogRevision: binding.catalogRevision,
              configRevision: binding.configRevision,
              authGeneration: binding.authGeneration,
            })),
          )
          .sort((a, b) =>
            a.modelToolName < b.modelToolName ? -1 : a.modelToolName > b.modelToolName ? 1 : 0,
          );
        return {
          bindingHash: snapshot.bindingHash,
          bindingEpoch: String(snapshot.bindingEpoch),
          bindingTools,
          drain: snapshot.drain ? drainProto(snapshot.drain) : undefined,
          denyHosts: snapshot.managedDenyPolicy.hosts,
          denyRevision: String(snapshot.managedDenyPolicy.revision),
          denyDigest: snapshot.managedDenyPolicy.digest,
          connections: snapshot.immutableBindings.map((binding) => ({
            connectionId: binding.connectionId,
            alias: binding.alias,
            endpoint: binding.config.endpoint,
            zone: binding.config.zone,
            credentialIdentity: binding.credentialIdentity,
            configRevision: binding.configRevision,
            authGeneration: binding.authGeneration,
            catalogRevision: binding.catalogRevision,
          })),
        };
      });
    },
    async rebindMcp(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      if (actor.serviceAccount)
        throw new ConnectError("a human decision is required", Code.PermissionDenied);
      const runtime = requireMcpHook(services().runtime);
      if (!req.expectedBindingHash || !req.expectedCandidateHash)
        throw new ConnectError(
          "binding and reviewed candidate hashes are required",
          Code.InvalidArgument,
        );
      return mcpOperation(() =>
        runtime.rebind(req.sessionId, req.expectedBindingHash, req.expectedCandidateHash, actor.id),
      );
    },
  };
}
