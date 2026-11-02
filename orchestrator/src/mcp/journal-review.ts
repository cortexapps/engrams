import { McpAuthorityStale } from "./authority-error.ts";
import {
  create,
  fromJson,
  toJson,
  type JsonValue,
  type MessageInitShape,
} from "@bufbuild/protobuf";
import { and, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  user,
  task,
  taskSession,
  profile,
  mcpExecution,
  mcpApproval,
  integrationConnection,
  mcpCatalog,
} from "../db/schema.ts";
import { isServiceAccountEmail } from "../rpc/api-key.ts";
import {
  McpWritePolicyRefSchema,
  McpTrustedWritePolicySchema,
} from "../gen/engram/app/v1/session_pb.ts";
import type { RequestPolicyContext } from "./session-policy.ts";
import type { ResolvedBoundTool } from "./session-resolver.ts";
import { restoreBindingSnapshot } from "./binding-store.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";

type Db = ReturnType<typeof getDb>;
export type JournalTx = Parameters<Parameters<Db["transaction"]>[0]>[0];
export type FrozenRequestContext = {
  resolvedTool: ResolvedBoundTool;
  requestPolicyContext: Omit<RequestPolicyContext, "acceptedPolicyRef" | "ancestorPolicyRefs"> & {
    acceptedPolicyRef: JsonValue;
    ancestorPolicyRefs: JsonValue[];
  };
};
export type JournalReviewContext = {
  executionId: string;
  sessionId: string;
  runId: string;
  toolCallId: string;
  requestDigest: string;
  argumentHash: string;
  bindingHash: string;
  connectionId: string;
  catalogId: string;
  catalogRevision: number;
  configRevision: number;
  authGeneration: number;
  credentialIdentity: string;
  upstreamName: string;
  toolFingerprint: string;
  contextDigest: string;
  journalRevision: number;
  expiresAt: string;
  policyRevision: string;
  bindingEpoch: string;
};
export function freezeRequestContext(
  resolvedTool: ResolvedBoundTool,
  policy: RequestPolicyContext,
): FrozenRequestContext {
  const result = {
    resolvedTool: structuredClone(resolvedTool),
    requestPolicyContext: {
      ...policy,
      acceptedPolicyRef: toJson(McpWritePolicyRefSchema, policy.acceptedPolicyRef),
      ancestorPolicyRefs: policy.ancestorPolicyRefs.map((ref) =>
        toJson(McpWritePolicyRefSchema, ref),
      ),
    },
  };
  if (Buffer.byteLength(canonicalJson(result)) > 2 * MCP_LIMITS.manifestBytes)
    throw new Error("request context exceeds limit");
  return JSON.parse(canonicalJson(result));
}
export function requestPolicyContext(context: FrozenRequestContext): RequestPolicyContext {
  const policy = context.requestPolicyContext;
  return {
    ...policy,
    acceptedPolicyRef: fromJson(McpWritePolicyRefSchema, policy.acceptedPolicyRef),
    ancestorPolicyRefs: policy.ancestorPolicyRefs.map((ref) =>
      fromJson(McpWritePolicyRefSchema, ref),
    ),
  };
}
/** Compare the complete frozen mode-policy lineage; a leaf revision alone is not enough. */
export function policyReviewCurrent(
  context: FrozenRequestContext,
  current: MessageInitShape<typeof McpTrustedWritePolicySchema>,
): boolean {
  const policy = create(McpTrustedWritePolicySchema, current);
  return (
    Boolean(policy.ref) &&
    canonicalDigest(context.requestPolicyContext.acceptedPolicyRef) ===
      canonicalDigest(toJson(McpWritePolicyRefSchema, policy.ref!)) &&
    canonicalDigest(context.requestPolicyContext.ancestorPolicyRefs) ===
      canonicalDigest(policy.ancestorPolicyRefs.map((ref) => toJson(McpWritePolicyRefSchema, ref)))
  );
}
export function assertOriginalArguments(original: string, expectedHash: string) {
  if (
    typeof original !== "string" ||
    Buffer.byteLength(original) > MCP_LIMITS.argumentBytes ||
    Buffer.from(original).toString("utf8") !== original
  )
    throw new Error("invalid argument bytes");
  let parsed: unknown;
  try {
    parsed = JSON.parse(original);
  } catch {
    throw new Error("invalid argument JSON");
  }
  if (
    !parsed ||
    typeof parsed !== "object" ||
    Array.isArray(parsed) ||
    canonicalDigest(parsed) !== expectedHash
  )
    throw new Error("argument digest mismatch");
}
/** Shared locks serialize authorization with owner, role, ban and session changes. */
export async function lockHumanSession(
  tx: JournalTx,
  sessionId: string,
  actorId: string,
  includeAncestors = false,
  sessionLock: "share" | "update" = "share",
) {
  const [actor] = await tx.select().from(user).where(eq(user.id, actorId)).for("share");
  if (!actor || actor.banned || isServiceAccountEmail(actor.email))
    throw new McpAuthorityStale("human session owner or administrator required");
  const { owner, session, ancestors } = await lockSessionChain(
    tx,
    sessionId,
    includeAncestors,
    sessionLock,
  );
  if (actor.role !== "admin" && owner.createdByUserId !== actorId)
    throw new McpAuthorityStale("human session owner or administrator required");
  return { owner, session, actor, ancestors };
}
export async function lockSessionChain(
  tx: JournalTx,
  sessionId: string,
  includeAncestors = true,
  sessionLock: "share" | "update" = "share",
) {
  const chain: { sessionId: string; taskId: string; parentId: string | null }[] = [];
  let id: string | null = sessionId;
  const seen = new Set<string>();
  while (id) {
    if (seen.has(id) || chain.length >= 32)
      throw new McpAuthorityStale("stale MCP review ancestry");
    seen.add(id);
    const refs = await tx
      .select({ taskId: taskSession.taskId, parentId: task.spawningSessionId })
      .from(taskSession)
      .innerJoin(task, eq(task.id, taskSession.taskId))
      .where(eq(taskSession.sessionId, id))
      .limit(2);
    if (refs.length !== 1)
      throw new McpAuthorityStale("human session owner or administrator required");
    chain.push({ sessionId: id, ...refs[0]! });
    id = includeAncestors ? refs[0]!.parentId : null;
  }
  // Global order: user -> sorted tasks -> sorted sessions -> sorted profiles/connections -> execution.
  const tasks = new Map<string, typeof task.$inferSelect>();
  for (const taskId of [...new Set(chain.map((ref) => ref.taskId))].sort()) {
    const [row] = await tx.select().from(task).where(eq(task.id, taskId)).for("share");
    if (!row) throw new McpAuthorityStale("human session owner or administrator required");
    tasks.set(taskId, row);
  }
  const sessions = new Map<string, typeof taskSession.$inferSelect>();
  for (const ref of [...chain].sort((a, b) => a.sessionId.localeCompare(b.sessionId))) {
    const [row] = await tx
      .select()
      .from(taskSession)
      .where(and(eq(taskSession.sessionId, ref.sessionId), eq(taskSession.taskId, ref.taskId)))
      .for(sessionLock);
    // Revalidate the discovered chain after all relevant writes are blocked.
    if (!row || tasks.get(ref.taskId)?.spawningSessionId !== ref.parentId)
      throw new McpAuthorityStale("stale MCP review ancestry");
    sessions.set(ref.sessionId, row);
  }
  const owner = tasks.get(chain[0]!.taskId)!,
    session = sessions.get(sessionId)!;
  return {
    owner,
    session,
    ancestors: chain
      .slice(1)
      .map((ref) => ({ session: sessions.get(ref.sessionId)!, owner: tasks.get(ref.taskId)! })),
  };
}
export type McpReviewCall = Pick<
  typeof mcpExecution.$inferSelect,
  "requestContext" | "originalArguments" | "bindingHash"
>;
export async function assertLocalReviewCurrent(
  tx: JournalTx,
  call: McpReviewCall,
  session: typeof taskSession.$inferSelect,
  ancestors: { session: typeof taskSession.$inferSelect; owner: typeof task.$inferSelect }[],
  connectionLock: "share" | "update" = "share",
) {
  if (!call.requestContext || call.originalArguments === null || !session.mcpBindingEnvelope)
    throw new McpAuthorityStale("original request context unavailable");
  const stored = restoreBindingSnapshot(
    session.mcpBindingEnvelope,
    session.integrationConnections ?? [],
  );
  const bound = call.requestContext.resolvedTool;
  const policy = requestPolicyContext(call.requestContext);
  if (
    stored.bindingHash !== call.bindingHash ||
    canonicalDigest(stored.profilePublication ?? null) !==
      canonicalDigest(bound.profilePublication ?? null) ||
    BigInt(stored.bindingEpoch) !== policy.acceptedPolicyRef.bindingEpoch ||
    canonicalDigest(stored.ancestorAuthorityRefs) !==
      canonicalDigest(bound.ancestorAuthorityRefs) ||
    !stored.immutableBindings.some(
      (binding) => canonicalDigest(binding) === canonicalDigest(bound.binding),
    )
  )
    throw new McpAuthorityStale("stale MCP review binding");
  const snapshots = [stored];
  const [leafTask] = await tx.select().from(task).where(eq(task.id, session.taskId));
  let parentId = leafTask?.spawningSessionId;
  const seen = new Set([session.sessionId]);
  if (stored.ancestorAuthorityRefs.length >= 32)
    throw new McpAuthorityStale("stale MCP review ancestry");
  for (let i = 0; i < stored.ancestorAuthorityRefs.length; i++) {
    const expected = stored.ancestorAuthorityRefs[i]!;
    if (parentId !== expected.sessionId || seen.has(parentId))
      throw new McpAuthorityStale("stale MCP review ancestry");
    seen.add(parentId);
    const ancestor = ancestors[i]?.session,
      ancestorTask = ancestors[i]?.owner;
    if (
      !ancestor?.mcpBindingEnvelope ||
      !ancestorTask ||
      ancestor.sessionId !== parentId ||
      ["done", "failed", "cancelled"].includes(ancestorTask.status)
    )
      throw new McpAuthorityStale("stale MCP review ancestry");
    const snapshot = restoreBindingSnapshot(
      ancestor.mcpBindingEnvelope,
      ancestor.integrationConnections ?? [],
    );
    if (
      snapshot.bindingHash !== expected.bindingHash ||
      snapshot.bindingEpoch !== expected.bindingEpoch ||
      canonicalDigest(snapshot.ancestorAuthorityRefs) !==
        canonicalDigest(stored.ancestorAuthorityRefs.slice(i + 1))
    )
      throw new McpAuthorityStale("stale MCP review ancestry");
    snapshots.push(snapshot);
    parentId = ancestorTask.spawningSessionId;
  }
  if (parentId || ancestors.length !== stored.ancestorAuthorityRefs.length)
    throw new McpAuthorityStale("stale MCP review ancestry");
  const profileIds = [...new Set(snapshots.map((snapshot) => snapshot.profileId))].sort();
  for (const id of profileIds) {
    const [currentProfile] = await tx.select().from(profile).where(eq(profile.id, id)).for("share");
    if (
      !currentProfile ||
      currentProfile.deletedAt ||
      snapshots
        .filter((snapshot) => snapshot.profileId === id)
        .some(
          (snapshot) =>
            canonicalDigest(
              (currentProfile.integrationGrants ?? []).filter((grant) => grant.mcp !== undefined),
            ) !== snapshot.profileGrantsHash ||
            canonicalDigest(currentProfile.mcpGrantPublication) !==
              canonicalDigest(snapshot.profilePublication ?? null),
        )
    )
      throw new McpAuthorityStale("stale MCP review profile");
  }
  const bindings = snapshots.flatMap((snapshot) => snapshot.immutableBindings);
  for (const id of [...new Set(bindings.map((binding) => binding.connectionId))].sort()) {
    const [live] = await tx
      .select()
      .from(integrationConnection)
      .where(eq(integrationConnection.id, id))
      .for(connectionLock);
    for (const binding of bindings.filter((binding) => binding.connectionId === id)) {
      const [catalog] = await tx
        .select()
        .from(mcpCatalog)
        .where(eq(mcpCatalog.id, binding.catalogId));
      if (
        !live ||
        live.provider !== "mcp" ||
        !live.enabled ||
        live.mcpCredential?.transition ||
        live.revoked ||
        live.alias !== binding.alias ||
        canonicalDigest(live.config) !== canonicalDigest(binding.config) ||
        live.configRevision !== binding.configRevision ||
        live.authGeneration !== binding.authGeneration ||
        live.credentialIdentity !== binding.credentialIdentity ||
        live.catalogRevision !== binding.catalogRevision ||
        !catalog ||
        catalog.kind !== "published" ||
        catalog.connectionId !== id ||
        catalog.revision !== binding.catalogRevision ||
        catalog.configRevision !== binding.configRevision ||
        catalog.authGeneration !== binding.authGeneration ||
        catalog.credentialIdentity !== binding.credentialIdentity ||
        binding.tools.some(
          (bound) =>
            !catalog.tools.some((tool) => canonicalDigest(tool) === canonicalDigest(bound.tool)),
        )
      )
        throw new McpAuthorityStale("stale MCP review connection");
    }
  }
}
export function reviewContext(
  call: typeof mcpExecution.$inferSelect,
  approval: typeof mcpApproval.$inferSelect,
): JournalReviewContext {
  if (!call.requestContext || call.originalArguments === null)
    throw new McpAuthorityStale("original request context unavailable");
  const { resolvedTool } = call.requestContext,
    policy = requestPolicyContext(call.requestContext);
  return {
    executionId: call.id,
    sessionId: call.sessionId,
    runId: call.runId,
    toolCallId: call.toolCallId,
    requestDigest: call.requestDigest,
    argumentHash: call.argumentHash,
    bindingHash: call.bindingHash,
    connectionId: call.connectionId,
    catalogId: call.catalogId,
    catalogRevision: resolvedTool.binding.catalogRevision,
    configRevision: call.configRevision,
    authGeneration: call.authGeneration,
    credentialIdentity: resolvedTool.binding.credentialIdentity,
    upstreamName: call.upstreamName,
    toolFingerprint: resolvedTool.reviewedTool.fingerprint,
    contextDigest: canonicalDigest(call.requestContext),
    journalRevision: call.version,
    expiresAt: approval.expiresAt.toISOString(),
    policyRevision: policy.acceptedPolicyRef.policyRevision.toString(),
    bindingEpoch: policy.acceptedPolicyRef.bindingEpoch.toString(),
  };
}
