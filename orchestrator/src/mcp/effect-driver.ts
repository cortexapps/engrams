import { McpAuthorityStale } from "./authority-error.ts";
import { fromJson } from "@bufbuild/protobuf";
import { and, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { integrationConnection, mcpExecution, taskSession } from "../db/schema.ts";
import { CredentialStatusSchema } from "../gen/engram/mcp/v1/control_pb.ts";
import { digest } from "../../../mcp-worker/src/index.ts";
import type {
  WorkerIdentity,
  WorkerIntent,
  WorkerOperation,
} from "../../../mcp-worker/src/contracts.ts";
import type {
  CredentialControlClient,
  CredentialControlTls,
  SessionPolicyControlClient,
} from "./control-client.ts";
import { makeCredentialBroker } from "./credential-broker.ts";
import { makeMcpActiveWatch } from "./active-watch.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { makeMcpJournal, McpUnknownWrite, type McpLiveDispatchAuthorization } from "./journal.ts";
import { requestPolicyContext } from "./journal-review.ts";
import { MCP_LIMITS } from "./limits.ts";
import { makeSessionWritePolicy, type RequestPolicyContext } from "./session-policy.ts";
import {
  resolveSnapshotTool,
  type SessionBindingResolver,
  type ResolvedBoundTool,
} from "./session-resolver.ts";
import { hasPublishedLeaf, type McpSessionBindingSnapshot } from "./session-bindings.ts";
import { makeResourceWorkerClient } from "./worker-client.ts";
import { toWorkerBindingHash } from "./worker-encoding.ts";

type Call = typeof mcpExecution.$inferSelect;
type Lease = NonNullable<Awaited<ReturnType<ReturnType<typeof makeMcpJournal>["getAdmission"]>>>;
export interface McpToolRequest {
  sessionId: string;
  runId: string;
  toolCallId: string;
  modelToolName: string;
  /** Exact first coordinator tool_call_requested.args_json, not parsed and serialized again. */
  originalArguments: string;
}
const requestDenials = {
  invalid_arguments: "MCP arguments invalid or exceed limit",
  tool_unavailable: "MCP tool unavailable",
  publication_required: "MCP profile grants require administrator publication",
  disabled: "external MCP is disabled",
  session_unavailable: "MCP session unavailable",
  authority_changed: "MCP execution authority changed",
  principal_unavailable: "MCP principal unavailable",
  write_not_allowed: "MCP write authority unavailable",
  deadline_expired: "MCP request deadline expired",
} as const;
export type McpRequestDenialCode = keyof typeof requestDenials;
/** A known local rejection, before any execution effect. No raw values or underlying error. */
export class McpRequestDenied extends Error {
  constructor(readonly code: McpRequestDenialCode) {
    super(requestDenials[code]);
  }
}
export class McpRequestConflict extends Error {
  readonly code = "existing_request";
  constructor() {
    super("MCP call identity changed");
  }
}
class InvalidArguments extends Error {
  constructor(readonly lease: Lease) {
    super("MCP arguments do not satisfy approved schemas");
  }
}
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function argumentsValue(raw: string): Record<string, unknown> {
  try {
    if (Buffer.byteLength(raw) > MCP_LIMITS.argumentBytes)
      throw new McpRequestDenied("invalid_arguments");
    const value: unknown = JSON.parse(raw);
    if (!object(value)) throw new McpRequestDenied("invalid_arguments");
    return value;
  } catch {
    throw new McpRequestDenied("invalid_arguments");
  }
}
function publication(snapshot: McpSessionBindingSnapshot) {
  if (!hasPublishedLeaf(snapshot)) throw new McpRequestDenied("publication_required");
}

/** The authenticated worker envelope is verified by the client and broker first. */
export function verifiedExecutionResult(value: unknown): {
  state: "succeeded" | "failed" | "outcome_unknown";
  result: Record<string, unknown>;
  resultUnavailable: boolean;
} {
  if (
    !object(value) ||
    Object.keys(value).some((key) => !["outcome", "delivery", "output", "reason"].includes(key)) ||
    !["succeeded", "failed", "outcome_unknown"].includes(String(value.outcome)) ||
    !["available", "unavailable"].includes(String(value.delivery))
  )
    throw Error("MCP result invalid");
  const state = value.outcome;
  if (state !== "succeeded" && state !== "failed" && state !== "outcome_unknown")
    throw Error("MCP result invalid");
  if (value.delivery === "available") {
    const output = value.output;
    if (
      state === "outcome_unknown" ||
      value.reason !== undefined ||
      !object(output) ||
      Object.keys(output).some(
        (key) => !["content", "structuredContent", "isError"].includes(key),
      ) ||
      !Array.isArray(output.content) ||
      output.content.some(
        (item: unknown) =>
          !object(item) ||
          Object.keys(item).some((key) => !["type", "text"].includes(key)) ||
          item.type !== "text" ||
          typeof item.text !== "string",
      ) ||
      (output.isError !== undefined && typeof output.isError !== "boolean") ||
      (state === "failed" ? output.isError !== true : output.isError === true)
    )
      throw Error("MCP result invalid");
    if (Buffer.byteLength(canonicalJson(value)) > MCP_LIMITS.resultBytes) {
      return {
        state,
        result: { outcome: state, delivery: "unavailable", reason: "result_limit" },
        resultUnavailable: state === "succeeded",
      };
    }
  } else if (
    value.output !== undefined ||
    !["unsupported_content", "result_limit", "output_schema", "upstream_failure"].includes(
      String(value.reason),
    ) ||
    (state === "outcome_unknown" && value.reason !== "upstream_failure")
  )
    throw Error("MCP result invalid");
  return {
    state,
    result: structuredClone(value),
    resultUnavailable: state === "succeeded" && value.delivery === "unavailable",
  };
}

/** Record verified truth independently of guest delivery and local cancellation. */
export async function persistVerifiedOutcome(
  journal: ReturnType<typeof makeMcpJournal>,
  dispatched: Call,
  outcome: ReturnType<typeof verifiedExecutionResult>,
  now: () => Date,
  verified = true,
): Promise<Call> {
  // Cancellation can change dispatching to unknown once. Only the result CAS is retried.
  for (let attempt = 0; attempt < 2; attempt++) {
    const latest = await journal.get(dispatched.id);
    if (!latest || latest.intentId !== dispatched.intentId)
      throw Error("MCP result identity changed");
    if (!["dispatching", "outcome_unknown"].includes(latest.state)) return latest;
    if (latest.state === "outcome_unknown" && (!verified || outcome.state === "outcome_unknown"))
      return latest;
    try {
      return await journal.finish(
        dispatched.id,
        latest.version,
        outcome.state,
        outcome.result,
        outcome.resultUnavailable,
        now(),
      );
    } catch (error) {
      if (attempt === 1) throw error;
    }
  }
  throw Error("MCP result commit unavailable");
}

/** Trusted effect driver. Call directly, never inside a replayable DBOS/SQL step.
 * Task 6 supplies operator/host enforcement and trusted zone TLS configuration. */
export function makeMcpEffectDriver(deps: {
  db?: ReturnType<typeof getDb>;
  deploymentId: string;
  control: CredentialControlClient;
  policy: SessionPolicyControlClient;
  resolver: SessionBindingResolver;
  workerTls(zoneId: string): CredentialControlTls;
  /** Fresh global and zone kill switches. Missing configuration stays disabled. */
  enabled?(zoneId: string): boolean;
  now(): Date;
}) {
  const db = deps.db ?? getDb(),
    journal = makeMcpJournal(db),
    policy = makeSessionWritePolicy(deps.policy);
  function enabled(resolved: ResolvedBoundTool, signal?: AbortSignal) {
    signal?.throwIfAborted();
    if (!deps.enabled?.(resolved.binding.config.zone)) throw new McpRequestDenied("disabled");
  }
  async function lookup(input: McpToolRequest) {
    const [call] = await db
      .select()
      .from(mcpExecution)
      .where(
        and(
          eq(mcpExecution.sessionId, input.sessionId),
          eq(mcpExecution.toolCallId, input.toolCallId),
        ),
      );
    if (
      call &&
      (call.runId !== input.runId ||
        call.originalArguments !== input.originalArguments ||
        call.requestContext?.resolvedTool.modelToolName !== input.modelToolName)
    )
      throw new McpRequestConflict();
    return call;
  }
  async function fence(context: RequestPolicyContext, resolved: ResolvedBoundTool, raw: string) {
    const expected = { modelToolName: resolved.modelToolName, originalArguments: raw };
    const status = await (resolved.reviewedTool.classification === "write"
      ? policy.validateDispatchFence(context, expected)
      : policy.validateExecutionIdentity(context, expected));
    if (status === "stale") throw new McpRequestDenied("authority_changed");
    if (status !== "current") throw Error("MCP policy fence unavailable");
  }
  async function current(call: Call, signal?: AbortSignal) {
    const resolved = call.requestContext?.resolvedTool;
    if (!resolved || call.originalArguments === null || call.deadline <= deps.now())
      throw Error("MCP request unavailable");
    enabled(resolved, signal);
    const snapshot = await deps.resolver.readCurrent(call.sessionId);
    publication(snapshot);
    if (snapshot.bindingHash !== call.bindingHash)
      throw new McpAuthorityStale("MCP binding changed");
    await journal.assertCurrent(call, deps.now());
    await fence(requestPolicyContext(call.requestContext!), resolved, call.originalArguments);
    enabled(resolved, signal);
    return resolved;
  }
  function client(resolved: ResolvedBoundTool, signal?: AbortSignal) {
    return makeResourceWorkerClient({
      tls: deps.workerTls(resolved.binding.config.zone),
      policy: resolved.binding.operatorPolicy,
      signal,
    });
  }
  function intent(
    call: Call,
    lease: Lease,
    identity: WorkerIdentity,
    operation: WorkerOperation,
  ): Omit<WorkerIntent, "operationDigest"> {
    const bound = call.requestContext!.resolvedTool.binding;
    return {
      id: crypto.randomUUID(),
      bootId: identity.bootId,
      zoneId: identity.zoneId,
      policyDigest: identity.policyDigest,
      connectionId: call.connectionId,
      connectionRevision: call.configRevision,
      credentialIdentity: bound.credentialIdentity,
      scope: {
        kind: "execution",
        sessionId: call.sessionId,
        runId: call.runId,
        toolCallId: call.toolCallId,
        bindingHash: toWorkerBindingHash(call.bindingHash),
      },
      operation: operation.kind,
      destination: "endpoint" in operation ? operation.endpoint : "",
      requestDigest: call.requestDigest,
      expiresAt: lease.expiresAt.getTime(),
      lease: {
        id: lease.reservationId,
        deploymentId: lease.deploymentId,
        expiresAt: lease.expiresAt.getTime(),
      },
    };
  }
  async function schema(call: Call, resolved: ResolvedBoundTool, signal?: AbortSignal) {
    if ((await journal.getAdmission(call.id))?.schemaDigest === call.requestDigest) return true;
    const operations: Extract<WorkerOperation, { kind: "validateSchemas" }>[] = [
      ...(resolved.reviewedTool.outputSchema
        ? [{ kind: "validateSchemas" as const, documents: [resolved.reviewedTool.outputSchema] }]
        : []),
      {
        kind: "validateSchemas",
        documents: [
          resolved.reviewedTool.inputSchema,
          ...resolved.effectiveGrant.argumentConstraintDocuments,
        ],
        value: argumentsValue(call.originalArguments!),
      },
    ];
    for (let i = 0; i < operations.length; i++) {
      if ((await journal.admitSchema(call.id, deps.deploymentId, deps.now())) === "queued")
        return false;
      const lease = (await journal.getAdmission(call.id))!;
      const worker = client(resolved, signal);
      let success = false;
      try {
        await current(call, signal);
        const identity = await worker.identity(),
          operation = operations[i]!;
        const response = await worker.dispatch({
          protocol: 1,
          intent: {
            ...intent(call, lease, identity, operation),
            operationDigest: digest(operation),
          },
          operation,
        });
        if (response.ok && response.result === false) throw new InvalidArguments(lease);
        if (!response.ok || response.result !== true) throw Error("MCP schema validation failed");
        success = true;
      } finally {
        worker.close();
        // Compile-only success releases capacity without claiming final argument proof.
        await journal.finishSchema(
          call.id,
          lease.reservationId,
          lease.expiresAt,
          call.requestDigest,
          success && i === operations.length - 1,
          deps.now(),
        );
      }
    }
    return true;
  }
  const activeWatch = makeMcpActiveWatch({
    intervalMs: 1_000,
    async check(id) {
      const call = await journal.get(id);
      if (!call || !["waiting_approval", "authorized", "dispatching"].includes(call.state))
        throw Error("MCP execution no longer active");
      await current(call);
    },
  });
  return {
    cancel(id: string) {
      activeWatch.cancel(id);
    },
    async reconcile(id: string, signal?: AbortSignal) {
      signal?.throwIfAborted();
      const call = await journal.get(id);
      if (!call || !["waiting_approval", "authorized", "dispatching"].includes(call.state)) return;
      if (call.deadline <= deps.now()) {
        signal?.throwIfAborted();
        activeWatch.cancel(id);
        await journal.cancelInternal(id, "deadline", deps.now());
        return;
      }
      try {
        await current(call, signal);
      } catch (error) {
        activeWatch.cancel(id);
        signal?.throwIfAborted();
        if (error instanceof McpRequestDenied || error instanceof McpAuthorityStale) {
          await journal.cancelInternal(id, "authority_changed", deps.now());
          return;
        }
        throw error;
      }
    },
    async acceptToolRequest(input: McpToolRequest, trustedDeadline?: Date) {
      input = structuredClone(input);
      const existing = await lookup(input);
      if (existing) return existing;
      const parsed = argumentsValue(input.originalArguments);
      // A transport failure must remain retryable, not become a durable missing-tool denial.
      const snapshot = await deps.resolver.readCurrent(input.sessionId);
      const resolved = resolveSnapshotTool(snapshot, input.modelToolName);
      if (!resolved) throw new McpRequestDenied("tool_unavailable");
      enabled(resolved);
      publication(snapshot);
      const currentPolicy = await policy.readForRequest(input);
      if (!currentPolicy.current || !currentPolicy.ref)
        throw new McpRequestDenied("session_unavailable");
      const context: RequestPolicyContext = {
        sessionId: input.sessionId,
        runId: input.runId,
        toolCallId: input.toolCallId,
        acceptedPolicyRef: currentPolicy.ref,
        acceptedWriteEligibility: currentPolicy.writeEligibility,
        ancestorPolicyRefs: currentPolicy.ancestorPolicyRefs,
      };
      if (
        resolved.reviewedTool.classification === "write" &&
        context.acceptedWriteEligibility !== "write_eligible"
      )
        throw new McpRequestDenied("write_not_allowed");
      await fence(context, resolved, input.originalArguments);
      const [session] = await db
        .select()
        .from(taskSession)
        .where(eq(taskSession.sessionId, input.sessionId));
      if (!session?.integrationPrincipalId) throw new McpRequestDenied("principal_unavailable");
      const write = resolved.reviewedTool.classification === "write";
      const authorization = !write
        ? { kind: "read" as const }
        : session.integrationPrincipalId.startsWith("automation:")
          ? { kind: "automation" as const, grant: resolved.effectiveGrant }
          : { kind: "approval" as const };
      const now = deps.now();
      // Both phase queues and approval share this first immutable outer cap.
      const cap =
        now.getTime() +
        (authorization.kind === "approval" ? MCP_LIMITS.approvalMs : 0) +
        MCP_LIMITS.queueMs +
        MCP_LIMITS.maxToolMs;
      const deadline = new Date(Math.min(cap, trustedDeadline?.getTime() ?? cap));
      if (!Number.isFinite(deadline.getTime()) || deadline <= now)
        throw new McpRequestDenied("deadline_expired");
      let call: Call;
      try {
        call = await journal.create(
          {
            taskId: session.taskId,
            ...input,
            principalId: session.integrationPrincipalId,
            connectionId: resolved.binding.connectionId,
            catalogId: resolved.binding.catalogId,
            bindingHash: resolved.bindingHash,
            argumentHash: canonicalDigest(parsed),
            resolvedTool: resolved,
            requestPolicyContext: context,
            upstreamName: resolved.reviewedTool.upstreamName,
            configRevision: resolved.binding.configRevision,
            authGeneration: resolved.binding.authGeneration,
            classification: resolved.reviewedTool.classification,
            deadline,
          },
          authorization,
          now,
        );
      } catch (error) {
        // A concurrent first acceptance owns the deadline and immutable context.
        const winner = await lookup(input);
        if (winner) return winner;
        if (error instanceof McpUnknownWrite) throw new McpRequestDenied("write_not_allowed");
        throw error;
      }
      await current(call);
      return call;
    },
    async driveOnce(id: string, signal?: AbortSignal): Promise<Call> {
      const watch = activeWatch.register(id);
      signal = signal ? AbortSignal.any([signal, watch.signal]) : watch.signal;
      try {
        let call = await journal.get(id);
        if (!call) throw Error("MCP call unavailable");
        if (!["waiting_approval", "authorized"].includes(call.state)) return call;
        await watch.ready;
        watch.throwIfFailed();
        signal.throwIfAborted();
        const resolved = await current(call, signal);
        try {
          if (!(await schema(call, resolved, signal))) return (await journal.get(id))!;
        } catch (error) {
          if (!(error instanceof InvalidArguments)) throw error;
          return journal.rejectBeforeDispatch(
            id,
            {
              requestDigest: call.requestDigest,
              version: call.version,
              reservation: {
                reservationId: error.lease.reservationId,
                expiresAt: error.lease.expiresAt,
              },
            },
            deps.now(),
          );
        }
        if (call.state === "waiting_approval") {
          await journal.requestApproval(id, deps.deploymentId, deps.now());
          return (await journal.get(id))!;
        }
        await current(call, signal);
        const [connection] = await db
          .select()
          .from(integrationConnection)
          .where(eq(integrationConnection.id, call.connectionId));
        if (!connection?.mcpCredential.status || connection.mcpCredential.transition)
          throw Error("MCP credential authority unavailable");
        const status = fromJson(CredentialStatusSchema, connection.mcpCredential.status);
        if (status.context?.authorizationGeneration !== BigInt(call.authGeneration))
          throw Error("MCP credential authority changed");
        if ((await journal.admit(id, deps.deploymentId, deps.now())) === "queued")
          return (await journal.get(id))!;
        const lease = (await journal.getAdmission(id))!;
        const worker = client(resolved, signal);
        try {
          const identity = await worker.identity();
          await current(call, signal);
          const operation: Extract<WorkerOperation, { kind: "execute" }> = {
            kind: "execute",
            endpoint: resolved.binding.config.endpoint,
            tool: resolved.reviewedTool.upstreamName,
            arguments: argumentsValue(call.originalArguments!),
            inputSchema: resolved.reviewedTool.inputSchema,
            ...(resolved.reviewedTool.outputSchema
              ? { outputSchema: resolved.reviewedTool.outputSchema }
              : {}),
            argumentConstraintDocuments: resolved.effectiveGrant.argumentConstraintDocuments,
            classification: resolved.reviewedTool.classification,
          };
          const dispatchIntent = intent(call, lease, identity, operation);
          const proof: McpLiveDispatchAuthorization = {
            executionId: id,
            requestDigest: call.requestDigest,
            bindingHash: call.bindingHash,
            principalId: call.principalId,
            expectedJournalRevision: call.version,
            reservationId: lease.reservationId,
            reservationExpiresAt: lease.expiresAt,
          };
          call = await journal.dispatch(
            id,
            { bootId: identity.bootId, intentId: dispatchIntent.id, expiresAt: lease.expiresAt },
            deps.now(),
            proof,
          );
          const dispatched = call;
          const broker = makeCredentialBroker({
            control: deps.control,
            now: () => deps.now().getTime(),
            admission: {
              async assertAdmitted() {
                await current(dispatched, signal);
                await journal.assertDispatchCurrent(id, proof, deps.now());
              },
            },
            worker,
            async beforeDispatch() {
              enabled(resolved, signal);
              await fence(
                requestPolicyContext(dispatched.requestContext!),
                resolved,
                dispatched.originalArguments!,
              );
              // The broker checks delivery expiry next, then sends on the retained socket.
              enabled(resolved, signal);
            },
          });
          let outcome: ReturnType<typeof verifiedExecutionResult>;
          let verified = true;
          try {
            const response = await broker.dispatch({
              status,
              identity,
              intent: dispatchIntent,
              operation,
            });
            if (!response.ok) throw Error("MCP result unavailable");
            outcome = verifiedExecutionResult(response.result);
          } catch {
            verified = false;
            const latest = await journal.get(call.id);
            if (!latest || latest.intentId !== dispatched.intentId)
              throw Error("MCP result identity changed");
            const interrupted = signal?.aborted || latest.state === "outcome_unknown";
            const unknown = interrupted || call.classification === "write";
            outcome = {
              state: unknown ? "outcome_unknown" : "failed",
              result: {
                outcome: unknown ? "outcome_unknown" : "failed",
                delivery: "unavailable",
                reason: interrupted ? "execution_interrupted" : "upstream_failure",
              },
              resultUnavailable: false,
            };
          }
          return persistVerifiedOutcome(journal, dispatched, outcome, deps.now, verified);
        } finally {
          worker.close();
        }
      } finally {
        watch.close();
      }
    },
  };
}
