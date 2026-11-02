import { and, asc, eq, gt, inArray, isNull, lte, sql } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  integrationConnection as connection,
  mcpCatalog as catalog,
  mcpExecution as execution,
  mcpApproval as approval,
  mcpResolution as resolution,
  mcpAdmission as admission,
  mcpAdmissionScope as scope,
  task,
} from "../db/schema.ts";
import { assertOwnerOrAdmin } from "./approvals.ts";
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
>;

async function lockScope(tx: Tx, id: string) {
  await tx.insert(scope).values({ id }).onConflictDoNothing();
  await tx.select().from(scope).where(eq(scope.id, id)).for("update");
}
async function owned(
  tx: Tx,
  call: typeof execution.$inferSelect,
  actorId: string,
  isAdmin: boolean,
) {
  const [owner] = await tx
    .select({ id: task.createdByUserId })
    .from(task)
    .where(eq(task.id, call.taskId));
  assertOwnerOrAdmin(owner?.id ?? null, actorId, isAdmin);
}
async function unknown(tx: Tx | Db, sessionId: string, connectionId: string): Promise<boolean> {
  const rows = await tx
    .select({ id: execution.id })
    .from(execution)
    .leftJoin(resolution, eq(resolution.executionId, execution.id))
    .where(
      and(
        eq(execution.sessionId, sessionId),
        eq(execution.connectionId, connectionId),
        eq(execution.classification, "write"),
        eq(execution.state, "outcome_unknown"),
        isNull(resolution.executionId),
      ),
    )
    .limit(1);
  return rows.length > 0;
}

export function makeMcpJournal(db = getDb()) {
  return {
    async get(id: string) {
      return (await db.select().from(execution).where(eq(execution.id, id)))[0] ?? null;
    },
    hasUnknown(sessionId: string, connectionId: string) {
      return unknown(db, sessionId, connectionId);
    },
    async create(input: McpCallInput, authorization: McpCallAuthorization, now: Date) {
      input = structuredClone(input);
      authorization = structuredClone(authorization);
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
          authorization.grant.upstreamName !== input.upstreamName
        )
          throw new Error("explicit automation write grant is required");
      }
      const state = authorization.kind === "approval" ? "waiting_approval" : "authorized";
      const authorizationKind = authorization.kind;
      const automationGrantHash =
        authorization.kind === "automation" ? canonicalDigest(authorization.grant) : null;
      const requestDigest = canonicalDigest({
        ...input,
        deadline: input.deadline.toISOString(),
        authorizationKind,
        automationGrantHash,
      });
      return db.transaction(async (tx) => {
        await tx
          .insert(execution)
          .values({
            ...input,
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
        const [existing] = await tx.select().from(approval).where(eq(approval.executionId, id));
        if (existing) return existing;
        const pending = await tx
          .select()
          .from(approval)
          .where(
            and(
              eq(approval.deploymentId, deploymentId),
              eq(approval.decision, "pending"),
              gt(approval.expiresAt, now),
            ),
          );
        if (
          pending.length >= MCP_LIMITS.deploymentApprovals ||
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
    async pending(sessionId: string, actorId: string, isAdmin: boolean, now: Date) {
      return db.transaction(async (tx) => {
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
          );
        for (const row of rows) await owned(tx, row.call, actorId, isAdmin);
        return rows;
      });
    },
    async decide(id: string, actorId: string, isAdmin: boolean, approve: boolean, now: Date) {
      return db.transaction(async (tx) => {
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (!call) throw new Error("call not found");
        await owned(tx, call, actorId, isAdmin);
        if (call.state !== "waiting_approval") throw new Error("approval is no longer pending");
        const rows = await tx
          .update(approval)
          .set({ decision: approve ? "approved" : "denied", actorId, decidedAt: now })
          .where(
            and(
              eq(approval.executionId, id),
              eq(approval.requestDigest, call.requestDigest),
              eq(approval.decision, "pending"),
              gt(approval.expiresAt, now),
            ),
          )
          .returning();
        if (!rows.length) throw new Error("approval expired or already decided");
        const [updated] = await tx
          .update(execution)
          .set({
            state: approve ? "authorized" : "canceled",
            version: call.version + 1,
            updatedAt: now,
          })
          .where(eq(execution.id, id))
          .returning();
        return updated!;
      });
    },
    async admit(
      id: string,
      deploymentId: string,
      now: Date,
      toolMs = MCP_LIMITS.toolMs,
    ): Promise<"queued" | "admitted"> {
      if (!Number.isSafeInteger(toolMs) || toolMs < 1 || toolMs > MCP_LIMITS.maxToolMs)
        throw new Error("invalid tool deadline");
      return db.transaction(async (tx) => {
        await lockScope(tx, deploymentId);
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (!call || call.state !== "authorized" || call.deadline <= now)
          throw new Error("call is not ready for admission");
        const rows = await tx
          .select()
          .from(admission)
          .where(eq(admission.deploymentId, deploymentId))
          .orderBy(asc(admission.createdAt));
        const existing = rows.find((a) => a.executionId === id);
        if (existing?.state === "admitted") return "admitted";
        if (existing && existing.expiresAt <= now) throw new Error("queue entry expired");
        // Expired active leases still count until expire() records the unknown outcome.
        const active = rows.filter((a) => a.state === "admitted");
        const available =
          active.length < MCP_LIMITS.deploymentConcurrency &&
          active.filter((a) => a.connectionId === call.connectionId).length <
            MCP_LIMITS.connectionConcurrency &&
          active.filter((a) => a.sessionId === call.sessionId).length <
            MCP_LIMITS.sessionConcurrency;
        const state = available ? "admitted" : "queued";
        if (
          !available &&
          !existing &&
          rows.filter((a) => a.state === "queued").length >= MCP_LIMITS.queueSize
        )
          throw new Error("MCP queue is full");
        const expiresAt = new Date(
          Math.min(
            call.deadline.getTime(),
            now.getTime() + (available ? toolMs : MCP_LIMITS.queueMs),
          ),
        );
        if (existing)
          await tx.update(admission).set({ state, expiresAt }).where(eq(admission.executionId, id));
        else
          await tx.insert(admission).values({
            executionId: id,
            deploymentId,
            sessionId: call.sessionId,
            connectionId: call.connectionId,
            state,
            expiresAt,
            createdAt: now,
          });
        return state;
      });
    },
    /** Irreversible dispatch barrier. The upstream effect runs after this commits. */
    async dispatch(
      id: string,
      intent: { bootId: string; intentId: string; expiresAt: Date },
      now: Date,
    ) {
      return db.transaction(async (tx) => {
        // Connection lock serializes unknown-outcome checks with finish/revoke.
        const [identity] = await tx
          .select({ connectionId: execution.connectionId })
          .from(execution)
          .where(eq(execution.id, id));
        if (!identity) throw new Error("call not found");
        const [live] = await tx
          .select()
          .from(connection)
          .where(eq(connection.id, identity.connectionId))
          .for("update");
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (!call || call.state !== "authorized" || call.deadline <= now)
          throw new Error("call cannot dispatch");
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
        if (
          call.classification === "write" &&
          (await unknown(tx, call.sessionId, call.connectionId))
        )
          throw new Error("unknown write outcome blocks this connection");
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
    async cancel(id: string, actorId: string, isAdmin: boolean, now: Date) {
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
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        await owned(tx, call!, actorId, isAdmin);
        if (!["waiting_approval", "authorized", "dispatching"].includes(call!.state)) return call!;
        const state: McpExecutionState =
          call!.state === "dispatching" ? "outcome_unknown" : "canceled";
        const [updated] = await tx
          .update(execution)
          .set({ state, version: call!.version + 1, updatedAt: now })
          .where(eq(execution.id, id))
          .returning();
        await tx.delete(admission).where(eq(admission.executionId, id));
        await tx
          .update(approval)
          .set({ decision: "denied", actorId, decidedAt: now })
          .where(and(eq(approval.executionId, id), eq(approval.decision, "pending")));
        return updated!;
      });
    },
    async resolveUnknown(
      id: string,
      actorId: string,
      isAdmin: boolean,
      outcome: "succeeded" | "failed" | "acknowledged",
      now: Date,
    ) {
      return db.transaction(async (tx) => {
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (!call || call.state !== "outcome_unknown")
          throw new Error("call has no unknown outcome");
        await owned(tx, call, actorId, isAdmin);
        const [row] = await tx
          .insert(resolution)
          .values({ executionId: id, actorId, outcome, createdAt: now })
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
            isNull(admission.executionId),
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
          if (lease) return false;
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
    async expire(deploymentId: string, now: Date) {
      // Lock order is scope -> connection -> execution, shared with admission/dispatch.
      return db.transaction(async (tx) => {
        await lockScope(tx, deploymentId);
        const expired = await tx
          .select()
          .from(admission)
          .where(and(eq(admission.deploymentId, deploymentId), lte(admission.expiresAt, now)));
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
          if (call && ["authorized", "dispatching"].includes(call.state))
            await tx
              .update(execution)
              .set({
                state: call.state === "dispatching" ? "outcome_unknown" : "canceled",
                version: call.version + 1,
                updatedAt: now,
              })
              .where(eq(execution.id, call.id));
          await tx.delete(admission).where(eq(admission.executionId, lease.executionId));
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
