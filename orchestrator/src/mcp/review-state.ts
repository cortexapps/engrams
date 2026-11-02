import { create, toBinary, toJson, type MessageInitShape } from "@bufbuild/protobuf";
import { GetSessionMcpStateResponseSchema } from "../gen/engram/app/v1/integration_pb.ts";
import { unknownContext, type McpJournal } from "./journal.ts";
import { reviewContext, policyReviewCurrent } from "./journal-review.ts";
import { restoreBindingSnapshot } from "./binding-store.ts";
import type { SessionBindingResolver } from "./session-resolver.ts";
import type { SessionPolicyControl } from "./session-policy.ts";

function stateSize(value: MessageInitShape<typeof GetSessionMcpStateResponseSchema>): number {
  const message = create(GetSessionMcpStateResponseSchema, value);
  return Math.max(
    toBinary(GetSessionMcpStateResponseSchema, message).byteLength,
    Buffer.byteLength(JSON.stringify(toJson(GetSessionMcpStateResponseSchema, message))),
  );
}

type SessionRows = Awaited<ReturnType<McpJournal["readSession"]>>;
export interface McpReviewProducers {
  bindings: Pick<SessionBindingResolver, "readCurrent">;
  policy: SessionPolicyControl;
}
export function approvalToProto(row: {
  call: SessionRows["rows"][number]["call"];
  approval: NonNullable<SessionRows["rows"][number]["approval"]>;
}) {
  const { call, approval } = row;
  if (!call.requestContext || call.originalArguments === null)
    throw new Error("original request context unavailable");
  const tool = call.requestContext.resolvedTool.reviewedTool;
  return {
    executionId: call.id,
    sessionId: call.sessionId,
    runId: call.runId,
    toolCallId: call.toolCallId,
    connectionId: call.connectionId,
    upstreamName: call.upstreamName,
    requestDigest: call.requestDigest,
    argumentHash: call.argumentHash,
    expiresAt: approval.expiresAt.toISOString(),
    originalArguments: call.originalArguments,
    reviewedTool: {
      ...tool,
      inputSchemaJson: JSON.stringify(tool.inputSchema),
      outputSchemaJson: tool.outputSchema ? JSON.stringify(tool.outputSchema) : "",
    },
    argumentConstraintDocumentsJson:
      call.requestContext.resolvedTool.effectiveGrant.argumentConstraintDocuments.map((document) =>
        JSON.stringify(document),
      ),
    expectedReviewContext: reviewContext(call, approval),
  };
}
/** Compose external freshness after the protected database read. No network inside PG locks. */
export async function sessionMcpState(input: {
  journal: McpJournal;
  sessionId: string;
  actorId: string;
  now: Date;
  after?: string;
  limit?: number;
  planToolCallId?: string;
  producers?: McpReviewProducers;
  canCancel?: boolean;
  canRebind?: boolean;
  afterBindingToolName?: string;
  bindingPageSize?: number;
  omitBindings?: boolean;
  omitExecutions?: boolean;
}) {
  const bindingLimit = input.bindingPageSize ?? 16;
  if (
    !Number.isSafeInteger(bindingLimit) ||
    bindingLimit < 1 ||
    bindingLimit > 16 ||
    (input.afterBindingToolName?.length ?? 0) > 128
  )
    throw new Error("invalid binding page");
  const data = await input.journal.readSession(
    input.sessionId,
    input.actorId,
    {
      after: input.after,
      limit: input.limit,
    },
    input.now,
  );
  const response: MessageInitShape<typeof GetSessionMcpStateResponseSchema> = {
    executions: [],
    bindingTools: [],
    approvals: [],
    unknownWriteBlocks: data.unknownWriteBlocks,
    resolutionReceipts: [],
    allowedActions: [],
    availability: "unavailable",
  };
  let current = false;
  const envelope = data.session.mcpBindingEnvelope;
  let snapshot: ReturnType<typeof restoreBindingSnapshot> | undefined;
  if (envelope) {
    response.bindingReceipt = {
      bindingHash: envelope.bindingHash,
      bindingEpoch: String(envelope.bindingEpoch),
      current: false,
      staleReasons: ["live_authority_unavailable"],
      allowedActions: [],
    };
    try {
      snapshot = restoreBindingSnapshot(envelope, data.session.integrationConnections ?? []);
      if (!input.producers?.policy.readWritePolicy) throw new Error("live producers missing");
      const live = await input.producers.bindings.readCurrent(input.sessionId);
      const policy = await input.producers.policy.readWritePolicy({
        protocol: 1,
        sessionId: input.sessionId,
        planToolCallId: input.planToolCallId,
      });
      if (
        !policy.state?.policy?.ref ||
        policy.state.policy.ref.sessionId !== input.sessionId ||
        live.bindingHash !== envelope.bindingHash
      )
        throw new Error("live authority changed");
      response.writePolicy = policy.state;
      response.writePolicy.allowedActions = policy.state.planReview ? ["approve_current_plan"] : [];
      current = true;
      response.bindingReceipt.current = true;
      response.bindingReceipt.staleReasons = [];
      response.availability = "available";
    } catch {
      response.bindingReceipt.staleReasons = ["binding_or_policy_unavailable"];
    }
  }
  if (input.canRebind && snapshot && response.bindingReceipt)
    response.bindingReceipt.allowedActions = ["rebind"];
  let last = input.after ?? "";
  for (const row of input.omitExecutions ? [] : data.rows) {
    const { call, approval, resolution, cancellation } = row;
    const actions: string[] = [];
    const review =
      approval && call.requestContext && call.originalArguments !== null
        ? approvalToProto({ call, approval })
        : undefined;
    let reason = call.requestContext ? "" : "original_request_unavailable";
    if (
      current &&
      review &&
      call.requestContext &&
      response.writePolicy?.policy &&
      policyReviewCurrent(call.requestContext, response.writePolicy.policy) &&
      approval?.decision === "pending" &&
      call.state === "waiting_approval" &&
      approval.expiresAt > input.now &&
      call.deadline > input.now &&
      response.writePolicy?.policy?.ref?.policyRevision?.toString() ===
        review.expectedReviewContext.policyRevision &&
      response.writePolicy.policy.ref.bindingEpoch?.toString() ===
        review.expectedReviewContext.bindingEpoch
    ) {
      actions.push("reject");
      if (
        call.requestContext?.requestPolicyContext.acceptedWriteEligibility === "write_eligible" &&
        response.writePolicy.policy.current &&
        response.writePolicy.policy.initialized &&
        response.writePolicy.policy.writeEligibility === "write_eligible"
      )
        actions.push("approve");
    } else if (call.state === "waiting_approval")
      reason = "review_expired_or_authority_unavailable";
    if (input.canCancel && ["waiting_approval", "authorized", "dispatching"].includes(call.state))
      actions.push("cancel");
    if (call.state === "outcome_unknown" && !resolution) actions.push("resolve_unknown");
    const frozenTool = call.requestContext?.resolvedTool.reviewedTool;
    const content =
      !review && frozenTool
        ? {
            originalArguments: call.originalArguments ?? "",
            reviewedTool: {
              ...frozenTool,
              inputSchemaJson: JSON.stringify(frozenTool.inputSchema),
              outputSchemaJson: frozenTool.outputSchema
                ? JSON.stringify(frozenTool.outputSchema)
                : "",
            },
            argumentConstraintDocumentsJson:
              call.requestContext!.resolvedTool.effectiveGrant.argumentConstraintDocuments.map(
                (document) => JSON.stringify(document),
              ),
          }
        : {};
    const entry = {
      ...content,
      expectedUnknownContext:
        call.state === "outcome_unknown" && call.intentId && !resolution
          ? unknownContext(call)
          : undefined,
      cancellationReceipt: cancellation
        ? {
            ...cancellation,
            actorId: cancellation.actorId ?? "",
            createdAt: cancellation.createdAt.toISOString(),
          }
        : undefined,
      bindingHash: call.bindingHash,
      executionId: call.id,
      connectionId: call.connectionId,
      state: call.state,
      journalRevision: call.version,
      resultUnavailable: call.resultUnavailable,
      review: actions.includes("approve") ? undefined : review,
      allowedActions: actions,
      reason,
      toolCallId: call.toolCallId,
      runId: call.runId,
      requestDigest: call.requestDigest,
      upstreamName: call.upstreamName,
      approvalDecision: approval?.decision ?? "",
    };
    const candidate = {
      ...response,
      executions: [...response.executions!, entry],
      approvals: [
        ...response.approvals!,
        ...(review && actions.includes("approve") ? [review] : []),
      ],
      resolutionReceipts: [
        ...response.resolutionReceipts!,
        ...(resolution
          ? [
              {
                executionId: resolution.executionId,
                actorId: resolution.actorId,
                outcome: resolution.outcome,
                createdAt: resolution.createdAt.toISOString(),
                expectedContext: resolution.expectedContext ?? undefined,
              },
            ]
          : []),
      ],
    };
    if (stateSize(candidate) > 2 * 1024 * 1024 - 256) {
      if (last === (input.after ?? "")) throw new Error("review exceeds state page limit");
      response.nextExecutionId = last;
      response.hasMoreExecutions = true;
      break;
    }
    Object.assign(response, candidate);
    last = call.id;
  }
  if (!input.omitExecutions && data.hasMore) {
    response.nextExecutionId = last;
    response.hasMoreExecutions = true;
  }
  if (snapshot && !input.omitBindings) {
    const tools = snapshot.immutableBindings
      .flatMap((binding) => binding.tools.map((bound) => ({ binding, bound })))
      .sort((a, b) => (a.bound.name < b.bound.name ? -1 : a.bound.name > b.bound.name ? 1 : 0))
      .filter(
        (item) => !input.afterBindingToolName || item.bound.name > input.afterBindingToolName,
      );
    let lastTool = input.afterBindingToolName ?? "",
      added = 0;
    for (const { binding, bound } of tools) {
      const summary = {
        connectionId: binding.connectionId,
        connectionAlias: binding.alias,
        modelToolName: bound.name,
        reviewedTool: {
          ...bound.tool,
          inputSchemaJson: JSON.stringify(bound.tool.inputSchema),
          outputSchemaJson: bound.tool.outputSchema ? JSON.stringify(bound.tool.outputSchema) : "",
        },
        argumentConstraintDocumentsJson: bound.grant.argumentConstraintDocuments.map((document) =>
          JSON.stringify(document),
        ),
        automationWrite: bound.grant.automationWrite,
        unrestricted: bound.grant.unrestricted,
        catalogId: binding.catalogId,
        catalogRevision: binding.catalogRevision,
        configRevision: binding.configRevision,
        authGeneration: binding.authGeneration,
      };
      const candidate = { ...response, bindingTools: [...response.bindingTools!, summary] };
      if (added === bindingLimit || stateSize(candidate) > 2 * 1024 * 1024 - 256) {
        if (input.omitExecutions && added === 0)
          throw new Error("binding review exceeds page limit");
        response.hasMoreBindingTools = true;
        response.nextBindingToolName = lastTool;
        break;
      }
      Object.assign(response, candidate);
      lastTool = bound.name;
      added++;
    }
  }
  if (stateSize(response) > 2 * 1024 * 1024) throw new Error("state exceeds limit");
  return response;
}
