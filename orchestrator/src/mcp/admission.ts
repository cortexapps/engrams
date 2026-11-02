import { and, asc, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  mcpAdmission as admission,
  mcpAdmissionScope as scope,
  mcpExecution as execution,
} from "../db/schema.ts";
import { MCP_LIMITS } from "./limits.ts";
import type { JournalTx } from "./journal-review.ts";

export async function lockAdmissionScope(tx: JournalTx, id: string) {
  await tx.insert(scope).values({ id }).onConflictDoNothing();
  await tx.select().from(scope).where(eq(scope.id, id)).for("update");
}
/** Both phases use the same durable capacity and retain their first queue deadline. */
export function makeMcpAdmission(db = getDb()) {
  return {
    async reserve(
      id: string,
      deploymentId: string,
      phase: "schema" | "execute",
      now: Date,
      toolMs = MCP_LIMITS.toolMs,
    ): Promise<"queued" | "admitted"> {
      if (!Number.isSafeInteger(toolMs) || toolMs < 1 || toolMs > MCP_LIMITS.maxToolMs)
        throw new Error("invalid tool deadline");
      return db.transaction(async (tx) => {
        await lockAdmissionScope(tx, deploymentId);
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        if (
          !call ||
          !call.requestContext ||
          call.originalArguments === null ||
          call.deadline <= now ||
          (phase === "execute"
            ? call.state !== "authorized"
            : !["waiting_approval", "authorized"].includes(call.state))
        )
          throw new Error("call is not ready for admission");
        const [existing] = await tx.select().from(admission).where(eq(admission.executionId, id));
        if (existing && existing.deploymentId !== deploymentId)
          throw new Error("admission deployment changed");
        if (phase === "execute" && existing?.schemaDigest !== call.requestDigest)
          throw new Error("verified schema is required");
        if (phase === "schema" && existing?.schemaDigest)
          throw new Error("schema already verified");
        if (existing?.state === "admitted") {
          if (existing.phase !== phase || existing.expiresAt <= now)
            throw new Error("admission lease unavailable");
          return "admitted";
        }
        const queueExpiresAt =
          (phase === "schema" ? existing?.schemaQueueExpiresAt : existing?.executeQueueExpiresAt) ??
          new Date(Math.min(call.deadline.getTime(), now.getTime() + MCP_LIMITS.queueMs));
        if (queueExpiresAt <= now) throw new Error("queue entry expired");
        const rows = await tx
          .select()
          .from(admission)
          .where(eq(admission.deploymentId, deploymentId))
          .orderBy(asc(admission.createdAt));
        const active = rows.filter((row) => row.state === "admitted");
        const available =
          active.length < MCP_LIMITS.deploymentConcurrency &&
          active.filter((row) => row.connectionId === call.connectionId).length <
            MCP_LIMITS.connectionConcurrency &&
          active.filter((row) => row.sessionId === call.sessionId).length <
            MCP_LIMITS.sessionConcurrency &&
          (phase !== "schema" || active.filter((row) => row.phase === "schema").length < 4);
        if (!available && existing?.state === "queued") return "queued";
        if (
          !available &&
          rows.filter((row) => row.state === "queued").length >= MCP_LIMITS.queueSize
        )
          throw new Error("MCP queue is full");
        const state = available ? "admitted" : "queued";
        const toolExpiresAt =
          phase === "execute" && available
            ? (existing?.executeExpiresAt ??
              new Date(Math.min(call.deadline.getTime(), now.getTime() + toolMs)))
            : undefined;
        if (toolExpiresAt && toolExpiresAt <= now) throw new Error("tool deadline expired");
        const values = {
          phase,
          state,
          // Every newly admitted operation has a single-use generation, including schema retries.
          reservationId:
            existing?.state === "queued" ? existing.reservationId : crypto.randomUUID(),
          expiresAt: available
            ? (toolExpiresAt ?? new Date(Math.min(call.deadline.getTime(), now.getTime() + 10_000)))
            : queueExpiresAt,
          ...(toolExpiresAt ? { executeExpiresAt: toolExpiresAt } : {}),
          ...(phase === "schema"
            ? { schemaQueueExpiresAt: queueExpiresAt }
            : { executeQueueExpiresAt: queueExpiresAt }),
        } as const;
        if (existing) await tx.update(admission).set(values).where(eq(admission.executionId, id));
        else
          await tx
            .insert(admission)
            .values({
              ...values,
              executionId: id,
              deploymentId,
              sessionId: call.sessionId,
              connectionId: call.connectionId,
              createdAt: now,
            });
        return state;
      });
    },
    /** Only a verified worker response may supply success. No public route exposes this method. */
    async finishSchema(
      id: string,
      reservationId: string,
      expiresAt: Date,
      requestDigest: string,
      success: boolean,
      now: Date,
    ) {
      const [observed] = await db.select().from(admission).where(eq(admission.executionId, id));
      if (!observed) throw new Error("schema admission missing");
      return db.transaction(async (tx) => {
        await lockAdmissionScope(tx, observed.deploymentId);
        const [call] = await tx.select().from(execution).where(eq(execution.id, id)).for("update");
        const [lease] = await tx
          .select()
          .from(admission)
          .where(eq(admission.executionId, id))
          .for("update");
        if (
          !call ||
          !["waiting_approval", "authorized"].includes(call.state) ||
          call.requestDigest !== requestDigest ||
          !lease ||
          lease.phase !== "schema" ||
          lease.state !== "admitted" ||
          lease.reservationId !== reservationId ||
          lease.expiresAt.getTime() !== expiresAt.getTime() ||
          (success && (lease.expiresAt <= now || call.deadline <= now))
        )
          throw new Error("stale schema admission");
        await tx
          .update(admission)
          .set({ state: "released", schemaDigest: success ? requestDigest : null })
          .where(and(eq(admission.executionId, id), eq(admission.reservationId, reservationId)));
      });
    },
  };
}
