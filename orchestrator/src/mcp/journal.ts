import { lockRuntimeAuthority } from "./runtime-authority.ts";
import { makeMcpAdmission, lockAdmissionScope as lockScope } from "./admission.ts";
import { and, asc, eq, gt, inArray, isNull, lte, or, sql } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  integrationConnection as connection,
  mcpCatalog as catalog,
  mcpExecution as execution,
  mcpApproval as approval,
  mcpResolution as resolution,
  mcpCancellation as cancellation,
  mcpAdmission as admission,
  taskSession,
} from "../db/schema.ts";
import {
  assertOriginalArguments,
  freezeRequestContext,
  lockHumanSession,
  lockSessionChain,
  assertLocalReviewCurrent,
  reviewContext,
  requestPolicyContext,
  type JournalReviewContext,
} from "./journal-review.ts";
import type { RequestPolicyContext } from "./session-policy.ts";
import type { ResolvedBoundTool } from "./session-resolver.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import { validateMcpGrant, type McpToolGrant, type McpExecutionState } from "./types.ts";

type Db = ReturnType<typeof getDb>;
type Tx = Parameters<Parameters<Db["transaction"]>[0]>[0];
export type McpCallAuthorization =
  | { kind: "read" }
  | { kind: "approval" }
  | { kind: "automation"; grant: McpToolGrant };
export type McpCallInput = Pick<
  typeof execution.$inferInsert,
  | "taskId"
  | "sessionId"
  | "runId"
  | "toolCallId"
  | "connectionId"
  | "catalogId"
  | "principalId"
  | "bindingHash"
  | "argumentHash"
  | "upstreamName"
  | "configRevision"
  | "authGeneration"
  | "classification"
  | "deadline"
> & {
  originalArguments: string;
  resolvedTool: ResolvedBoundTool;
  requestPolicyContext: RequestPolicyContext;
};

function unknownBlocks(now: Date) {
  return or(
    isNull(resolution.executionId),
    isNull(execution.intentId),
    isNull(execution.intentExpiresAt),
    gt(execution.intentExpiresAt, now),
  );
}
async function unknown(
  tx: Tx | Db,
  sessionIds: string[],
  connectionId: string,
  now: Date,
): Promise<boolean> {
  const rows = await tx
    .select({ id: execution.id })
    .from(execution)
    .leftJoin(resolution, eq(resolution.executionId, execution.id))
    .where(
      and(
        inArray(execution.sessionId, sessionIds),
        eq(execution.connectionId, connectionId),
        eq(execution.classification, "write"),
        eq(execution.state, "outcome_unknown"),
        unknownBlocks(now),
      ),
    )
    .limit(1);
  return rows.length > 0;
}

export class McpUnknownWrite extends Error {
  constructor() {
    super("unknown write outcome blocks this connection");
  }
}

/** Only the chain revalidated under the runtime locks can extend the write restriction. */
async function assertNoUnknownWrite(
  tx: Tx,
  call: Pick<typeof execution.$inferSelect, "classification"> & { connectionId: string },
  authority: Awaited<ReturnType<typeof lockRuntimeAuthority>>,
  now: Date,
) {
  if (
    call.classification === "write" &&
    (await unknown(
      tx,
      [authority.session.sessionId, ...authority.ancestors.map(({ session }) => session.sessionId)],
      call.connectionId,
      now,
    ))
  )
    throw new McpUnknownWrite();
}

/** Internal runtime proof; no public or guest route accepts this value. The runtime obtains it
 * from its fresh principal, full binding/ancestry, schema, policy and kill checks. */
export interface McpLiveDispatchAuthorization {
  executionId: string;
  requestDigest: string;
  bindingHash: string;
  principalId: string;
  expectedJournalRevision: number;
  reservationId: string;
  reservationExpiresAt: Date;
}
export type McpUnknownContext = {
  executionId: string;
  journalRevision: number;
  requestDigest: string;
  bindingHash: string;
  intentId: string;
};
export function unknownContext(call: typeof execution.$inferSelect): McpUnknownContext {
  if (call.state !== "outcome_unknown" || !call.intentId)
    throw Error("call has no unknown outcome");
  return {
    executionId: call.id,
    journalRevision: call.version,
    requestDigest: call.requestDigest,
    bindingHash: call.bindingHash,
    intentId: call.intentId,
  };
}
export type McpInternalCancellationCause =
  | "interrupt"
  | "end"
  | "revoke"
  | "kill"
  | "deadline"
  | "resume"
  | "authority_changed";
export function makeMcpJournal(db = getDb()) {
  const phasedAdmission = makeMcpAdmission(db);
  async function cancelExecution(
    id: string,
    authority:
      | { kind: "human"; actorId: string; expectedJournalRevision: number }
      | { kind: "internal"; cause: McpInternalCancellationCause },
    now: Date,
  ) {
    return db.transaction(async (tx) => {
      const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
      if (!identity) throw Error("call not found");
      if (authority.kind === "human")
        await lockHumanSession(tx, identity.sessionId, authority.actorId);
      await tx
        .select()
        .from(connection)
        .where(eq(connection.id, identity.connectionId))
        .for("update");
      const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
      if (!call) throw Error("call not found");
      const [prior] = await tx.select().from(cancellation).where(eq(cancellation.executionId, id));
      if (authority.kind === "human") {
        if (
          prior?.actorId === authority.actorId &&
          prior.expectedJournalRevision === authority.expectedJournalRevision
        )
          return call;
        if (
          !Number.isSafeInteger(authority.expectedJournalRevision) ||
          call.version !== authority.expectedJournalRevision
        )
          throw Error("stale cancellation context");
      }
      if (!["waiting_approval", "authorized", "dispatching"].includes(call.state)) {
        if (authority.kind === "human") throw Error("stale cancellation context");
        return call;
      }
      const state = call.state === "dispatching" ? "outcome_unknown" : "canceled";
      const [updated] = await tx
        .update(execution)
        .set({ state, version: call.version + 1, updatedAt: now })
        .where(eq(execution.id, id))
        .returning();
      await tx.insert(cancellation).values({
        executionId: id,
        actorId: authority.kind === "human" ? authority.actorId : null,
        cause: authority.kind === "human" ? "human" : authority.cause,
        expectedJournalRevision: call.version,
        journalRevision: call.version + 1,
        state,
        createdAt: now,
      });
      await tx.delete(admission).where(eq(admission.executionId, id));
      await tx
        .update(approval)
        .set({
          decision: "denied",
          actorId: authority.kind === "human" ? authority.actorId : null,
          decidedAt: now,
        })
        .where(and(eq(approval.executionId, id), eq(approval.decision, "pending")));
      return updated!;
    });
  }

  return {
    admitSchema: (id: string, deploymentId: string, now: Date) =>
      phasedAdmission.reserve(id, deploymentId, "schema", now),
    finishSchema: phasedAdmission.finishSchema,
    async get(id: string) {
      return (await db.select().from(execution).where(eq(execution.id, id)))[0] ?? null;
    },
    async getAdmission(id: string) {
      return (await db.select().from(admission).where(eq(admission.executionId, id)))[0] ?? null;
    },
    hasUnknown(sessionId: string, connectionId: string, now: Date = new Date()) {
      return unknown(db, [sessionId], connectionId, now);
    },
    async assertCurrent(call: typeof execution.$inferSelect, now: Date) {
      return db.transaction(async (tx) => {
        const authority = await lockRuntimeAuthority(tx, call, "share");
        await assertNoUnknownWrite(tx, call, authority, now);
      });
    },
    async create(input: McpCallInput, authorization: McpCallAuthorization, now: Date) {
      input = structuredClone(input);
      authorization = structuredClone(authorization);
      assertOriginalArguments(input.originalArguments, input.argumentHash);
      const context = freezeRequestContext(input.resolvedTool, input.requestPolicyContext);
      const { resolvedTool, requestPolicyContext: policy, ...storedInput } = input;
      const bound = resolvedTool.binding;
      if (
        policy.sessionId !== input.sessionId ||
        policy.runId !== input.runId ||
        policy.toolCallId !== input.toolCallId ||
        policy.acceptedPolicyRef.sessionId !== input.sessionId ||
        policy.acceptedPolicyRef.bindingEpoch < 0n ||
        policy.acceptedPolicyRef.bindingEpoch > BigInt(Number.MAX_SAFE_INTEGER) ||
        policy.ancestorPolicyRefs.length !== resolvedTool.ancestorAuthorityRefs.length ||
        policy.ancestorPolicyRefs.some(
          (ref, index) =>
            ref.sessionId !== resolvedTool.ancestorAuthorityRefs[index]?.sessionId ||
            ref.bindingEpoch !== BigInt(resolvedTool.ancestorAuthorityRefs[index]!.bindingEpoch),
        ) ||
        resolvedTool.bindingHash !== input.bindingHash ||
        bound.connectionId !== input.connectionId ||
        bound.catalogId !== input.catalogId ||
        bound.configRevision !== input.configRevision ||
        bound.authGeneration !== input.authGeneration ||
        resolvedTool.reviewedTool.upstreamName !== input.upstreamName ||
        resolvedTool.reviewedTool.classification !== input.classification ||
        !bound.tools.some(
          (tool) =>
            tool.name === resolvedTool.modelToolName &&
            canonicalDigest(tool.tool) === canonicalDigest(resolvedTool.reviewedTool) &&
            canonicalDigest(tool.grant) === canonicalDigest(resolvedTool.effectiveGrant),
        )
      )
        throw new Error("request review identity mismatch");
      if (
        ![input.argumentHash, input.bindingHash].every((hash) => /^sha256:[0-9a-f]{64}$/.test(hash))
      )
        throw new Error("canonical binding and argument hashes are required");
      if (
        input.deadline <= now ||
        input.deadline.getTime() - now.getTime() >
          MCP_LIMITS.approvalMs + MCP_LIMITS.queueMs + MCP_LIMITS.maxToolMs ||
        !input.runId ||
        !input.toolCallId ||
        !input.principalId
      )
        throw new Error("invalid call identity or deadline");
      if (authorization.kind === "read" && input.classification !== "read")
        throw new Error("write requires approval or an explicit automation grant");
      if (authorization.kind === "automation") {
        validateMcpGrant({
          connectionId: input.connectionId,
          operation: "invoke",
          resourceConstraints: [],
          credentialScope: "org",
          mcp: authorization.grant,
        });
        if (
          input.classification !== "write" ||
          !authorization.grant.automationWrite ||
          authorization.grant.upstreamName !== input.upstreamName ||
          canonicalDigest(authorization.grant) !== canonicalDigest(resolvedTool.effectiveGrant)
        )
          throw new Error("explicit automation write grant is required");
      }
      const state = authorization.kind === "approval" ? "waiting_approval" : "authorized";
      const authorizationKind = authorization.kind;
      const automationGrantHash =
        authorization.kind === "automation" ? canonicalDigest(authorization.grant) : null;
      const requestDigest = canonicalDigest({
        ...storedInput,
        requestContext: context,
        deadline: input.deadline.toISOString(),
        authorizationKind,
        automationGrantHash,
      });
      return db.transaction(async (tx) => {
        async function existing() {
          const [row] = await tx
            .select()
            .from(execution)
            .where(
              and(
                eq(execution.sessionId, input.sessionId),
                eq(execution.toolCallId, input.toolCallId),
              ),
            );
          if (row && row.requestDigest !== requestDigest) throw new Error("call digest mismatch");
          return row;
        }
        // Duplicates retain the original request and outcome even after authority changes.
        const prior = await existing();
        if (prior) return prior;
        const authority = await lockSessionChain(tx, input.sessionId);
        if (authority.session.taskId !== input.taskId)
          throw new Error("stored binding unavailable");
        await assertLocalReviewCurrent(
          tx,
          { ...storedInput, requestContext: context },
          authority.session,
          authority.ancestors,
          "update",
        );
        // A concurrent acceptance can commit while this request waits for the connection.
        const winner = await existing();
        if (winner) return winner;
        await assertNoUnknownWrite(tx, input, authority, now);
        await tx
          .insert(execution)
          .values({
            ...storedInput,
            requestContext: context,
            id: crypto.randomUUID(),
            state,
            authorizationKind,
            automationGrantHash,
            requestDigest,
            createdAt: now,
            updatedAt: now,
          })
          .onConflictDoNothing({ target: [execution.sessionId, execution.toolCallId] });
        const [row] = await tx
          .select()
          .from(execution)
          .where(
            and(
              eq(execution.sessionId, input.sessionId),
              eq(execution.toolCallId, input.toolCallId),
            ),
          );
        if (!row || row.requestDigest !== requestDigest) throw new Error("call digest mismatch");
        return row;
      });
    },
    async requestApproval(id: string, deploymentId: string, now: Date) {
      return db.transaction(async (tx) => {
        await lockScope(tx, deploymentId);
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (!call || call.state !== "waiting_approval" || call.deadline <= now)
          throw new Error("call is not pending approval");
        const [schema] = await tx.select().from(admission).where(eq(admission.executionId, id));
        if (!schema || schema.schemaDigest !== call.requestDigest || schema.state !== "released")
          throw new Error("verified released schema is required");
        const [existing] = await tx.select().from(approval).where(eq(approval.executionId, id));
        if (existing) return existing;
        const pending = await tx
          .select()
          .from(approval)
          .where(
            and(
              or(eq(approval.deploymentId, deploymentId), eq(approval.sessionId, call.sessionId)),
              eq(approval.decision, "pending"),
              gt(approval.expiresAt, now),
            ),
          );
        if (
          pending.filter((a) => a.deploymentId === deploymentId).length >=
            MCP_LIMITS.deploymentApprovals ||
          pending.filter((a) => a.sessionId === call.sessionId).length >=
            MCP_LIMITS.sessionApprovals
        )
          throw new Error("approval capacity reached");
        const [row] = await tx
          .insert(approval)
          .values({
            executionId: id,
            deploymentId,
            sessionId: call.sessionId,
            requestDigest: call.requestDigest,
            decision: "pending",
            expiresAt: new Date(
              Math.min(call.deadline.getTime(), now.getTime() + MCP_LIMITS.approvalMs),
            ),
          })
          .returning();
        return row!;
      });
    },
    async pending(sessionId: string, actorId: string, now: Date) {
      return db.transaction(async (tx) => {
        await lockHumanSession(tx, sessionId, actorId);
        const rows = await tx
          .select({ call: execution, approval })
          .from(execution)
          .innerJoin(approval, eq(approval.executionId, execution.id))
          .where(
            and(
              eq(execution.sessionId, sessionId),
              eq(approval.decision, "pending"),
              gt(approval.expiresAt, now),
            ),
          )
          .orderBy(asc(execution.id))
          .limit(MCP_LIMITS.sessionApprovals);
        return rows.filter((row) => row.call.requestContext && row.call.originalArguments !== null);
      });
    },
    async readExecutionResult(sessionId: string, executionId: string, actorId: string) {
      return db.transaction(async (tx) => {
        await lockHumanSession(tx, sessionId, actorId);
        const [call] = await tx
          .select()
          .from(execution)
          .where(and(eq(execution.sessionId, sessionId), eq(execution.id, executionId)));
        if (!call) throw new Error("call not found");
        return call;
      });
    },
    async readSession(
      sessionId: string,
      actorId: string,
      page: { after?: string; limit?: number } = {},
      now: Date = new Date(),
    ) {
      const limit = page.limit ?? 16;
      if (
        !Number.isSafeInteger(limit) ||
        limit < 1 ||
        limit > 16 ||
        (page.after?.length ?? 0) > 128
      )
        throw new Error("invalid state page");
      return db.transaction(async (tx) => {
        const { session } = await lockHumanSession(tx, sessionId, actorId);
        const rows = await tx
          .select({ call: execution, approval, resolution, cancellation })
          .from(execution)
          .leftJoin(approval, eq(approval.executionId, execution.id))
          .leftJoin(resolution, eq(resolution.executionId, execution.id))
          .leftJoin(cancellation, eq(cancellation.executionId, execution.id))
          .where(
            and(
              eq(execution.sessionId, sessionId),
              page.after ? gt(execution.id, page.after) : undefined,
            ),
          )
          .orderBy(asc(execution.id))
          .limit(limit + 1);
        const blocks = await tx
          .selectDistinct({ connectionId: execution.connectionId })
          .from(execution)
          .leftJoin(resolution, eq(resolution.executionId, execution.id))
          .where(
            and(
              eq(execution.sessionId, sessionId),
              eq(execution.classification, "write"),
              eq(execution.state, "outcome_unknown"),
              unknownBlocks(now),
            ),
          )
          .limit(17);
        if (blocks.length > 16) throw new Error("unknown connection state exceeds limit");
        return {
          session,
          rows: rows.slice(0, limit),
          hasMore: rows.length > limit,
          unknownWriteBlocks: blocks.map((row) => row.connectionId),
        };
      });
    },
    async decide(
      id: string,
      actorId: string,
      approve: boolean,
      expected: JournalReviewContext,
      now: Date,
    ) {
      return db.transaction(async (tx) => {
        const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        const { session, owner, ancestors } = await lockHumanSession(
          tx,
          identity.sessionId,
          actorId,
          true,
        );
        // Lock profile before connection/execution; no network call is made in this transaction.
        await assertLocalReviewCurrent(tx, identity, session, ancestors);
        const [live] = await tx
          .select()
          .from(connection)
          .where(eq(connection.id, identity.connectionId))
          .for("share");
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        const [item] = await tx
          .select()
          .from(approval)
          .where(eq(approval.executionId, id))
          .for("update");
        if (
          !call ||
          call.requestDigest !== identity.requestDigest ||
          call.taskId !== owner.id ||
          call.state !== "waiting_approval" ||
          !item ||
          item.decision !== "pending" ||
          item.expiresAt <= now ||
          call.deadline <= now
        )
          throw new Error("approval expired or already decided");
        if (!expected || canonicalDigest(expected) !== canonicalDigest(reviewContext(call, item)))
          throw new Error("stale review context");
        if (
          approve &&
          call.classification === "write" &&
          requestPolicyContext(call.requestContext!).acceptedWriteEligibility !== "write_eligible"
        )
          throw new Error("request policy denies writes");
        const frozen = call.requestContext!.resolvedTool;
        const [review] = await tx.select().from(catalog).where(eq(catalog.id, call.catalogId));
        if (
          ["done", "failed", "cancelled"].includes(owner.status) ||
          !live?.enabled ||
          live.revoked ||
          live.alias !== frozen.binding.alias ||
          canonicalDigest(live.config) !== canonicalDigest(frozen.binding.config) ||
          live.configRevision !== call.configRevision ||
          live.authGeneration !== call.authGeneration ||
          live.credentialIdentity !== frozen.binding.credentialIdentity ||
          live.catalogRevision !== frozen.binding.catalogRevision ||
          !review ||
          review.kind !== "published" ||
          review.connectionId !== call.connectionId ||
          review.revision !== frozen.binding.catalogRevision ||
          !review.tools.some(
            (tool) => canonicalDigest(tool) === canonicalDigest(frozen.reviewedTool),
          )
        )
          throw new Error("stale MCP review authority");
        const [decision] = await tx
          .update(approval)
          .set({ decision: approve ? "approved" : "denied", actorId, decidedAt: now })
          .where(eq(approval.executionId, id))
          .returning();
        const [updated] = await tx
          .update(execution)
          .set({
            state: approve ? "authorized" : "canceled",
            version: call.version + 1,
            updatedAt: now,
          })
          .where(eq(execution.id, id))
          .returning();
        return { call: updated!, approval: decision!, reviewedContext: expected };
      });
    },
    admit: (id: string, deploymentId: string, now: Date, toolMs = MCP_LIMITS.toolMs) =>
      phasedAdmission.reserve(id, deploymentId, "execute", now, toolMs),
    /** Irreversible dispatch barrier. The upstream effect runs after this commits. */
    async dispatch(
      id: string,
      intent: { bootId: string; intentId: string; expiresAt: Date },
      now: Date,
      liveAuthorization: McpLiveDispatchAuthorization,
    ) {
      liveAuthorization = structuredClone(liveAuthorization);
      intent = structuredClone(intent);
      const [reservation] = await db.select().from(admission).where(eq(admission.executionId, id));
      if (!reservation) throw new Error("valid admission is required");
      try {
        return await db.transaction(async (tx) => {
          await lockScope(tx, reservation.deploymentId);
          const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
          if (!identity) throw new Error("call not found");
          // Scope -> principal -> tasks/sessions -> profiles -> sorted connections -> call.
          const authority = await lockRuntimeAuthority(tx, identity);
          const [live] = await tx
            .select()
            .from(connection)
            .where(eq(connection.id, identity.connectionId))
            .for("update");
          const [call] = await tx
            .select()
            .from(execution)
            .where(eq(execution.id, id))
            .for("update");
          if (!call || call.state !== "authorized" || call.deadline <= now)
            throw new Error("call cannot dispatch");
          if (
            !call.requestContext ||
            call.originalArguments === null ||
            !liveAuthorization ||
            liveAuthorization.executionId !== id ||
            liveAuthorization.requestDigest !== call.requestDigest ||
            liveAuthorization.bindingHash !== call.bindingHash ||
            liveAuthorization.principalId !== call.principalId ||
            liveAuthorization.expectedJournalRevision !== call.version
          )
            throw new Error("stale dispatch authorization");
          const frozenPolicy = requestPolicyContext(call.requestContext);
          if (
            call.classification === "write" &&
            frozenPolicy.acceptedWriteEligibility !== "write_eligible"
          )
            throw new Error("request policy denies writes");
          const [review] = await tx.select().from(catalog).where(eq(catalog.id, call.catalogId));
          if (
            !live ||
            live.provider !== "mcp" ||
            !live.enabled ||
            live.revoked ||
            live.configRevision !== call.configRevision ||
            live.authGeneration !== call.authGeneration ||
            !review ||
            review.connectionId !== live.id ||
            review.kind !== "published" ||
            review.revision !== live.catalogRevision ||
            review.configRevision !== call.configRevision ||
            review.authGeneration !== call.authGeneration ||
            review.credentialIdentity !== live.credentialIdentity
          )
            throw new Error("stale MCP authority");
          const reviewedTool = review.tools.find((t) => t.upstreamName === call.upstreamName);
          if (
            !reviewedTool ||
            !("classification" in reviewedTool) ||
            reviewedTool.classification !== call.classification ||
            !("enabled" in reviewedTool) ||
            !reviewedTool.enabled
          )
            throw new Error("tool is not approved for this operation");
          await assertNoUnknownWrite(tx, call, authority, now);
          const [lease] = await tx
            .select()
            .from(admission)
            .where(
              and(
                eq(admission.executionId, id),
                eq(admission.state, "admitted"),
                gt(admission.expiresAt, now),
              ),
            );
          if (
            !lease ||
            lease.phase !== "execute" ||
            lease.schemaDigest !== call.requestDigest ||
            lease.reservationId !== reservation.reservationId ||
            lease.reservationId !== liveAuthorization.reservationId ||
            lease.expiresAt.getTime() !== liveAuthorization.reservationExpiresAt.getTime() ||
            !intent.bootId ||
            !intent.intentId ||
            intent.expiresAt <= now ||
            intent.expiresAt > lease.expiresAt
          )
            throw new Error("valid admission and intent are required");
          const [decision] = await tx.select().from(approval).where(eq(approval.executionId, id));
          if (call.authorizationKind === "approval" && !decision)
            throw new Error("approval is required");
          if (decision) {
            const consumed = await tx
              .update(approval)
              .set({ consumedAt: now })
              .where(
                and(
                  eq(approval.executionId, id),
                  eq(approval.requestDigest, call.requestDigest),
                  eq(approval.decision, "approved"),
                  isNull(approval.consumedAt),
                  gt(approval.expiresAt, now),
                ),
              )
              .returning();
            if (!consumed.length) throw new Error("approval expired or consumed");
          }
          const [updated] = await tx
            .update(execution)
            .set({
              state: "dispatching",
              version: call.version + 1,
              bootId: intent.bootId,
              intentId: intent.intentId,
              intentExpiresAt: intent.expiresAt,
              updatedAt: now,
            })
            .where(eq(execution.id, id))
            .returning();
          return updated!;
        });
      } catch (error) {
        // Release only a pre-barrier reservation. A concurrent committed dispatch keeps its lease.
        await db.transaction(async (tx) => {
          await lockScope(tx, reservation.deploymentId);
          const [call] = await tx
            .select()
            .from(execution)
            .where(eq(execution.id, id))
            .for("update");
          if (
            call?.state === "authorized" &&
            reservation.reservationId === liveAuthorization?.reservationId &&
            reservation.expiresAt.getTime() === liveAuthorization.reservationExpiresAt.getTime()
          )
            await tx
              .update(admission)
              .set({ state: "released" })
              .where(
                and(
                  eq(admission.executionId, id),
                  eq(admission.reservationId, reservation.reservationId),
                  eq(admission.expiresAt, reservation.expiresAt),
                ),
              );
        });
        throw error;
      }
    },
    /** Both credential-broker checks use the same committed generation and barrier revision. */
    async assertDispatchCurrent(id: string, proof: McpLiveDispatchAuthorization, now: Date) {
      const [observed] = await db.select().from(admission).where(eq(admission.executionId, id));
      if (!observed) throw new Error("dispatch admission unavailable");
      return db.transaction(async (tx) => {
        await lockScope(tx, observed.deploymentId);
        const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        const authority = await lockRuntimeAuthority(tx, identity, "share");
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("share");
        const [lease] = await tx
          .select()
          .from(admission)
          .where(eq(admission.executionId, id))
          .for("share");
        if (
          !call ||
          call.state !== "dispatching" ||
          call.deadline <= now ||
          !call.intentExpiresAt ||
          call.intentExpiresAt <= now ||
          proof.executionId !== id ||
          proof.requestDigest !== call.requestDigest ||
          proof.bindingHash !== call.bindingHash ||
          proof.principalId !== call.principalId ||
          proof.expectedJournalRevision + 1 !== call.version ||
          !lease ||
          lease.phase !== "execute" ||
          lease.state !== "admitted" ||
          lease.schemaDigest !== call.requestDigest ||
          lease.reservationId !== proof.reservationId ||
          lease.expiresAt.getTime() !== proof.reservationExpiresAt.getTime() ||
          lease.expiresAt <= now
        )
          throw new Error("stale dispatch admission");
        if (call.authorizationKind === "approval") {
          const [decision] = await tx
            .select()
            .from(approval)
            .where(eq(approval.executionId, id))
            .for("share");
          if (
            !decision ||
            decision.decision !== "approved" ||
            !decision.consumedAt ||
            decision.requestDigest !== call.requestDigest ||
            decision.expiresAt <= now
          )
            throw new Error("dispatch approval unavailable");
        }
        await assertNoUnknownWrite(tx, call, authority, now);
        return call;
      });
    },
    /** Trusted local validation failure, before any irreversible dispatch. */
    async rejectBeforeDispatch(
      id: string,
      expected: {
        requestDigest: string;
        version: number;
        reservation: { reservationId: string; expiresAt: Date } | null;
      },
      now: Date,
    ) {
      expected = structuredClone(expected);
      const [observed] = await db.select().from(admission).where(eq(admission.executionId, id));
      return db.transaction(async (tx) => {
        if (observed) await lockScope(tx, observed.deploymentId);
        const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        await tx
          .select()
          .from(connection)
          .where(eq(connection.id, identity.connectionId))
          .for("update");
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        const [lease] = await tx
          .select()
          .from(admission)
          .where(eq(admission.executionId, id))
          .for("update");
        if (
          !call ||
          !["waiting_approval", "authorized"].includes(call.state) ||
          call.version !== expected.version ||
          call.requestDigest !== expected.requestDigest ||
          call.intentId ||
          (expected.reservation
            ? !observed ||
              !lease ||
              lease.deploymentId !== observed.deploymentId ||
              lease.reservationId !== expected.reservation.reservationId ||
              lease.expiresAt.getTime() !== expected.reservation.expiresAt.getTime()
            : Boolean(lease))
        )
          throw new Error("stale pre-dispatch rejection");
        const [updated] = await tx
          .update(execution)
          .set({
            state: "failed",
            version: call.version + 1,
            result: {
              source: "local_validation",
              delivery: "available",
              output: {
                content: [
                  { type: "text", text: "Tool arguments do not satisfy the approved schemas." },
                ],
                isError: true,
              },
            },
            resultUnavailable: false,
            updatedAt: now,
          })
          .where(eq(execution.id, id))
          .returning();
        await tx.delete(admission).where(eq(admission.executionId, id));
        await tx
          .update(approval)
          .set({ decision: "denied", decidedAt: now })
          .where(
            and(
              eq(approval.executionId, id),
              isNull(approval.consumedAt),
              inArray(approval.decision, ["pending", "approved"]),
            ),
          );
        return updated!;
      });
    },
    async finish(
      id: string,
      version: number,
      state: "succeeded" | "failed" | "outcome_unknown",
      result: Record<string, unknown> | null,
      resultUnavailable: boolean,
      now: Date,
    ) {
      if (result && Buffer.byteLength(canonicalJson(result)) > MCP_LIMITS.resultBytes)
        throw new Error("result exceeds limit");
      if (state === "succeeded" && result === null && !resultUnavailable)
        throw new Error("success needs a verified result or an explicit unavailable result");
      if (state !== "succeeded" && resultUnavailable)
        throw new Error("unavailable result requires known success");
      return db.transaction(async (tx) => {
        const [identity] = await tx
          .select({ connectionId: execution.connectionId })
          .from(execution)
          .where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        await tx
          .select()
          .from(connection)
          .where(eq(connection.id, identity.connectionId))
          .for("update");
        const [row] = await tx
          .update(execution)
          .set({ state, result, resultUnavailable, version: version + 1, updatedAt: now })
          .where(
            and(
              eq(execution.id, id),
              eq(execution.version, version),
              state === "outcome_unknown"
                ? eq(execution.state, "dispatching")
                : inArray(execution.state, ["dispatching", "outcome_unknown"]),
            ),
          )
          .returning();
        if (!row) throw new Error("stale execution transition");
        await tx.delete(admission).where(eq(admission.executionId, id));
        return row;
      });
    },
    cancel(id: string, actorId: string, now: Date, expectedJournalRevision: number) {
      return cancelExecution(id, { kind: "human", actorId, expectedJournalRevision }, now);
    },
    /** Trusted server lifecycle only; never supplied through a public request. */
    cancelInternal(id: string, cause: McpInternalCancellationCause, now: Date) {
      return cancelExecution(id, { kind: "internal", cause }, now);
    },
    async cancellationReceipt(id: string) {
      return (
        (await db.select().from(cancellation).where(eq(cancellation.executionId, id)))[0] ?? null
      );
    },
    async resolveUnknown(
      id: string,
      actorId: string,
      outcome: "succeeded" | "failed" | "acknowledged",
      now: Date,
      expected: McpUnknownContext,
    ) {
      return db.transaction(async (tx) => {
        const [identity] = await tx.select().from(execution).where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        await lockHumanSession(tx, identity.sessionId, actorId);
        await tx
          .select()
          .from(connection)
          .where(eq(connection.id, identity.connectionId))
          .for("update");
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        const [prior] = await tx.select().from(resolution).where(eq(resolution.executionId, id));
        if (
          prior &&
          prior.actorId === actorId &&
          prior.outcome === outcome &&
          canonicalDigest(prior.expectedContext) === canonicalDigest(expected)
        )
          return prior;
        if (
          !call ||
          call.state !== "outcome_unknown" ||
          !expected ||
          canonicalDigest(expected) !== canonicalDigest(unknownContext(call))
        )
          throw new Error("stale unknown outcome context");

        const [row] = await tx
          .insert(resolution)
          .values({ executionId: id, actorId, outcome, expectedContext: expected, createdAt: now })
          .onConflictDoNothing()
          .returning();
        if (!row) throw new Error("outcome already resolved");
        // Human resolution unblocks the connection but never invents an upstream result.
        return row;
      });
    },
    /** Recover a crash between call insertion and its approval/admission transaction. */
    async expireUnadmitted(now: Date, limit = MCP_LIMITS.queueSize) {
      if (!Number.isSafeInteger(limit) || limit < 1 || limit > MCP_LIMITS.queueSize)
        throw new Error("invalid expiry batch size");
      const candidates = await db
        .select({ id: execution.id })
        .from(execution)
        .leftJoin(admission, eq(admission.executionId, execution.id))
        .where(
          and(
            inArray(execution.state, ["waiting_approval", "authorized"]),
            lte(execution.deadline, now),
            sql`(${admission.executionId} IS NULL OR ${admission.state} = 'released')`,
          ),
        )
        .orderBy(asc(execution.deadline))
        .limit(limit);
      let expired = 0;
      // One call lock per transaction avoids lock-order cycles with the lease sweeper.
      for (const candidate of candidates) {
        const changed = await db.transaction(async (tx) => {
          const [call] = await tx
            .select()
            .from(execution)
            .where(eq(execution.id, candidate.id))
            .for("update");
          if (
            !call ||
            !["waiting_approval", "authorized"].includes(call.state) ||
            call.deadline > now
          )
            return false;
          // Re-read after the row lock. Admission can have committed while this step waited.
          const [lease] = await tx
            .select()
            .from(admission)
            .where(eq(admission.executionId, call.id));
          if (lease && lease.state !== "released") return false;
          await tx
            .update(execution)
            .set({ state: "canceled", version: call.version + 1, updatedAt: now })
            .where(eq(execution.id, call.id));
          await tx
            .update(approval)
            .set({ decision: "expired", decidedAt: now })
            .where(
              and(
                eq(approval.executionId, call.id),
                inArray(approval.decision, ["pending", "approved"]),
                isNull(approval.consumedAt),
              ),
            );
          return true;
        });
        if (changed) expired++;
      }
      return expired;
    },
    /** Stored scopes survive worker removal and disabled dispatch. Scan only due work. */
    async expiryScopes(now: Date, after = "") {
      const rows = await db.execute<{ deployment_id: string }>(sql`
        SELECT deployment_id FROM (
          SELECT ${admission.deploymentId} AS deployment_id FROM ${admission}
          WHERE ${admission.state} IN ('queued', 'admitted') AND ${admission.expiresAt} <= ${now}
          UNION
          SELECT ${approval.deploymentId} AS deployment_id FROM ${approval}
          WHERE ${approval.decision} IN ('pending', 'approved')
            AND ${approval.consumedAt} IS NULL AND ${approval.expiresAt} <= ${now}
        ) AS due WHERE deployment_id > ${after} ORDER BY deployment_id LIMIT 32
      `);
      return rows.rows.map((row) => row.deployment_id);
    },
    async expire(deploymentId: string, now: Date) {
      // Lock order is scope -> connection -> execution, shared with admission/dispatch.
      return db.transaction(async (tx) => {
        await lockScope(tx, deploymentId);
        const expired = await tx
          .select()
          .from(admission)
          .where(
            and(
              eq(admission.deploymentId, deploymentId),
              inArray(admission.state, ["queued", "admitted"]),
              lte(admission.expiresAt, now),
            ),
          );
        for (const lease of expired.sort((a, b) => a.connectionId.localeCompare(b.connectionId))) {
          await tx
            .select()
            .from(connection)
            .where(eq(connection.id, lease.connectionId))
            .for("update");
          const [call] = await tx
            .select()
            .from(execution)
            .where(eq(execution.id, lease.executionId))
            .for("update");
          if (
            lease.phase === "schema" &&
            lease.schemaQueueExpiresAt &&
            lease.schemaQueueExpiresAt > now
          ) {
            await tx
              .update(admission)
              .set({ state: "released" })
              .where(eq(admission.executionId, lease.executionId));
            continue;
          }
          if (call && ["waiting_approval", "authorized", "dispatching"].includes(call.state))
            await tx
              .update(execution)
              .set({
                state: call.state === "dispatching" ? "outcome_unknown" : "canceled",
                version: call.version + 1,
                updatedAt: now,
              })
              .where(eq(execution.id, call.id));
          await tx
            .update(admission)
            .set({ state: "released" })
            .where(eq(admission.executionId, lease.executionId));
        }
        const pending = await tx
          .select()
          .from(approval)
          .where(
            and(
              eq(approval.deploymentId, deploymentId),
              inArray(approval.decision, ["pending", "approved"]),
              isNull(approval.consumedAt),
              lte(approval.expiresAt, now),
            ),
          );
        for (const item of pending) {
          await tx.select().from(execution).where(eq(execution.id, item.executionId)).for("update");
          await tx
            .update(approval)
            .set({ decision: "expired", decidedAt: now })
            .where(and(eq(approval.executionId, item.executionId), isNull(approval.consumedAt)));
          await tx
            .update(execution)
            .set({ state: "canceled", version: sql`${execution.version} + 1`, updatedAt: now })
            .where(
              and(
                eq(execution.id, item.executionId),
                inArray(execution.state, ["waiting_approval", "authorized"]),
              ),
            );
        }
        return expired.length + pending.length;
      });
    },
  };
}
export type McpJournal = ReturnType<typeof makeMcpJournal>;
