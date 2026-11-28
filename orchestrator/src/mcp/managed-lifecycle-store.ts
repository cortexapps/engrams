import { and, asc, eq, gt, isNotNull, isNull, sql } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { task, taskSession } from "../db/schema.ts";
import { canonicalJson } from "./hash.ts";
import { managedSourceSchema, rustCreationIdentity } from "./managed-state.ts";
import { compileMcpManifest } from "./session-bindings.ts";
import type { ManagedLifecycle } from "./managed-lifecycle.ts";

function checked(value: ManagedLifecycle): ManagedLifecycle {
  if (
    value.version !== 1 ||
    value.nativeVersion !== 1 ||
    !/^sha256:[0-9a-f]{64}$/.test(value.nativeDigest) ||
    !/^sha256:[0-9a-f]{64}$/.test(value.identity.inputDigest) ||
    !/^sha256:[0-9a-f]{64}$/.test(value.bindingHash) ||
    !["reserving", "held", "preparing", "finalizing", "started", "recovering", "terminal"].includes(
      value.phase,
    ) ||
    Object.keys(value).some(
      (key) =>
        ![
          "version",
          "identity",
          "nativeVersion",
          "nativeDigest",
          "phase",
          "bindingHash",
          "sourceKey",
          "recovery",
          "pending",
        ].includes(key),
    ) ||
    Object.keys(value.identity).some(
      (key) =>
        ![
          "sessionId",
          "taskId",
          "inputId",
          "actorId",
          "parentSessionId",
          "writeEligibility",
          "inputDigest",
        ].includes(key),
    ) ||
    !value.identity.sessionId ||
    !value.identity.taskId ||
    !value.identity.actorId ||
    !value.identity.inputId ||
    !["read_only", "write_eligible"].includes(value.identity.writeEligibility) ||
    Buffer.byteLength(canonicalJson(value)) > 2 * 1024 * 1024
  )
    throw Error("MCP lifecycle identity unavailable");
  const identity = rustCreationIdentity(value.identity);
  for (const key of [value.sourceKey, value.recovery?.sourceKey]) {
    if (!key) continue;
    managedSourceSchema.parse(key);
    if (
      canonicalJson(key.creation) !== canonicalJson(identity) ||
      key.native_digest !== value.nativeDigest ||
      key.context.target.session_id !== value.identity.sessionId
    )
      throw Error("MCP lifecycle source changed");
  }
  if (value.pending) {
    const pending = value.pending;
    if (
      !value.sourceKey ||
      pending.snapshot.bindingHash !== value.bindingHash ||
      pending.snapshot.bindingEpoch !== value.sourceKey.context.target.binding_epoch ||
      !/^sha256:[0-9a-f]{64}$/.test(pending.combinedManifestDigest) ||
      !/^sha256:[0-9a-f]{64}$/.test(pending.preparedReceiptId) ||
      Object.keys(pending).some(
        (k) => !["snapshot", "combinedManifestDigest", "preparedReceiptId"].includes(k),
      )
    )
      throw Error("MCP lifecycle finalization changed");
    compileMcpManifest(pending.snapshot);
  }
  if (value.recovery) {
    const recovery = value.recovery;
    if (
      !/^[a-zA-Z0-9-]{1,128}$/.test(recovery.id) ||
      !recovery.actorId ||
      !/^sha256:[0-9a-f]{64}$/.test(recovery.expectedBindingHash) ||
      recovery.candidateHash !== recovery.candidate.bindingHash ||
      recovery.candidate.bindingEpoch !== recovery.sourceKey.context.target.binding_epoch + 1 ||
      Object.keys(recovery).some(
        (k) =>
          ![
            "id",
            "sourceKey",
            "expectedBindingHash",
            "candidateHash",
            "actorId",
            "candidate",
          ].includes(k),
      )
    )
      throw Error("MCP lifecycle recovery changed");
    compileMcpManifest(recovery.candidate);
  }
  return structuredClone(value);
}
/** Exact JSON and binding CAS; no transaction spans a coordinator or worker call. */
export function makeManagedLifecycleStore(db = getDb()) {
  async function read(sessionId: string) {
    const rows = await db
      .select({ session: taskSession, owner: task })
      .from(taskSession)
      .innerJoin(task, eq(task.id, taskSession.taskId))
      .where(eq(taskSession.sessionId, sessionId))
      .limit(2);
    if (rows.length !== 1) throw Error("MCP session reservation unavailable");
    const row = rows[0]!;
    if (row.session.mcpManagedLifecycle) checked(row.session.mcpManagedLifecycle);
    return row;
  }
  return {
    read,
    async initialize(value: ManagedLifecycle) {
      value = checked(value);
      if (value.phase !== "reserving" || value.sourceKey || value.recovery || value.pending)
        throw Error("MCP initial lifecycle unavailable");
      const { session, owner } = await read(value.identity.sessionId);
      if (
        session.taskId !== value.identity.taskId ||
        session.integrationPrincipalId !== value.identity.actorId ||
        owner.spawningSessionId !== (value.identity.parentSessionId ?? null) ||
        session.mcpBindingEnvelope?.bindingHash !== value.bindingHash ||
        session.mcpBindingEnvelope.bindingEpoch !== 0
      )
        throw Error("MCP reservation identity changed");
      if (session.mcpManagedLifecycle) {
        if (canonicalJson(session.mcpManagedLifecycle) !== canonicalJson(value))
          throw Error("MCP creation already reserved");
        return;
      }
      const rows = await db
        .update(taskSession)
        .set({ mcpManagedLifecycle: value })
        .where(
          and(
            eq(taskSession.taskId, value.identity.taskId),
            eq(taskSession.sessionId, value.identity.sessionId),
            eq(taskSession.integrationPrincipalId, value.identity.actorId),
            isNull(taskSession.mcpManagedLifecycle),
            sql`${taskSession.mcpBindingEnvelope}->>'bindingHash' = ${value.bindingHash}`,
          ),
        )
        .returning({ id: taskSession.sessionId });
      if (rows.length !== 1) throw Error("MCP creation reservation changed");
    },
    async compareAndSet(
      previous: ManagedLifecycle,
      next: ManagedLifecycle,
      columns?: Pick<
        typeof taskSession.$inferInsert,
        | "mcpBindingEnvelope"
        | "integrationConnections"
        | "integrationGrants"
        | "integrationSnapshotHash"
      >,
    ) {
      previous = checked(previous);
      next = checked(next);
      if (
        canonicalJson(previous.identity) !== canonicalJson(next.identity) ||
        previous.nativeDigest !== next.nativeDigest
      )
        throw Error("MCP immutable creation changed");
      if (
        (!columns && previous.bindingHash !== next.bindingHash) ||
        (columns && columns.mcpBindingEnvelope?.bindingHash !== next.bindingHash)
      )
        throw Error("MCP final binding changed");
      const rows = await db
        .update(taskSession)
        .set({ ...columns, mcpManagedLifecycle: next })
        .where(
          and(
            eq(taskSession.taskId, previous.identity.taskId),
            eq(taskSession.sessionId, previous.identity.sessionId),
            sql`${taskSession.mcpManagedLifecycle} = ${JSON.stringify(previous)}::jsonb`,
            sql`${taskSession.mcpBindingEnvelope}->>'bindingHash' = ${previous.bindingHash}`,
          ),
        )
        .returning({ id: taskSession.sessionId });
      if (rows.length !== 1) throw Error("MCP lifecycle changed");
    },
    async pending(after?: string) {
      return db
        .select({ sessionId: taskSession.sessionId })
        .from(taskSession)
        .where(
          and(
            isNotNull(taskSession.mcpManagedLifecycle),
            sql`${taskSession.mcpManagedLifecycle}->>'phase' NOT IN ('started','terminal')`,
            ...(after ? [gt(taskSession.sessionId, after)] : []),
          ),
        )
        .orderBy(asc(taskSession.sessionId))
        .limit(32);
    },
  };
}
