import { and, asc, eq, gt, inArray, isNull, or } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  integrationConnection,
  mcpExecution,
  profile,
  taskSession,
  type IntegrationConnectionSnapshot,
} from "../db/schema.ts";
import { lockAdmissionScope } from "./admission.ts";
import {
  bindingSnapshotColumns,
  readLiveBindingAuthority,
  restoreBindingSnapshot,
} from "./binding-store.ts";
import { canonicalDigest } from "./hash.ts";
import { lockHumanSession } from "./journal-review.ts";
import { applyRebindDeny, prepareRebindDeny, type McpRebindHost } from "./rebind-deny.ts";
import {
  compileMcpManifest,
  compileSessionBindings,
  managedAuthorityHash,
  type McpOperatorPolicy,
  type McpSessionBindingSnapshot,
} from "./session-bindings.ts";
import { expectedManagedDenyAck } from "./session-resolver.ts";

export type McpRebindDrain = {
  reason: "dispatch_in_progress" | "unknown_intent" | "legacy_intent";
  retryAfter: string;
};
export class McpRebindDraining extends Error {
  constructor(readonly drain: McpRebindDrain) {
    super("MCP dispatch in progress");
  }
}

/** External operations under the session lock have a hard limit. Host operations must not callback into O. */
async function bounded<T>(operation: () => Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation(),
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(Error("MCP rebind service unavailable")), 5_000);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

export function makeMcpRebind(deps: {
  db?: ReturnType<typeof getDb>;
  deploymentId: string;
  host?: McpRebindHost;
  readOperatorPolicies(): Promise<McpOperatorPolicy[]>;
  readBindingEpoch(id: string): Promise<number>;
  readParentCurrent(id: string): Promise<McpSessionBindingSnapshot>;
  now?: () => Date;
}) {
  const db = deps.db ?? getDb();
  async function run(sessionId: string, oldHash: string, actorId: string, candidateHash?: string) {
    if (!sessionId || !oldHash || !actorId || !deps.host)
      throw Error("MCP rebind service unavailable");
    const host = deps.host;
    return db.transaction(async (tx) => {
      await lockAdmissionScope(tx, deps.deploymentId);
      const { session, owner, ancestors } = await lockHumanSession(
        tx,
        sessionId,
        actorId,
        true,
        "update",
      );
      if (
        ["done", "failed", "cancelled"].includes(owner.status) ||
        !session.mcpBindingEnvelope ||
        !session.profileId
      )
        throw Error("MCP session unavailable");
      const old = restoreBindingSnapshot(
        session.mcpBindingEnvelope,
        session.integrationConnections ?? [],
      );
      if (old.bindingHash !== oldHash) throw Error("stale MCP binding");
      if (
        old.profileId !== session.profileId ||
        old.ancestorAuthorityRefs.length !== ancestors.length ||
        old.ancestorAuthorityRefs.some(
          (ref, index) => ref.sessionId !== ancestors[index]?.session.sessionId,
        )
      )
        throw Error("MCP immutable ancestry changed");
      const profileIds = [
        ...new Set([
          session.profileId,
          ...ancestors.map((row) => row.session.profileId).filter((id): id is string => !!id),
        ]),
      ].sort();
      await tx
        .select()
        .from(profile)
        .where(inArray(profile.id, profileIds))
        .orderBy(asc(profile.id))
        .for("share");
      const connections = await tx
        .select()
        .from(integrationConnection)
        .where(eq(integrationConnection.provider, "mcp"))
        .orderBy(asc(integrationConnection.id))
        .for("share");
      // A broker may already be beyond its last database check. The original intent
      // expiry bounds credential delivery even after cancellation or manual resolution.
      const [pending] = await tx
        .select({
          state: mcpExecution.state,
          intentId: mcpExecution.intentId,
          intentExpiresAt: mcpExecution.intentExpiresAt,
        })
        .from(mcpExecution)
        .where(
          and(
            eq(mcpExecution.sessionId, sessionId),
            or(
              eq(mcpExecution.state, "dispatching"),
              and(
                eq(mcpExecution.state, "outcome_unknown"),
                or(
                  isNull(mcpExecution.intentId),
                  isNull(mcpExecution.intentExpiresAt),
                  gt(mcpExecution.intentExpiresAt, (deps.now ?? (() => new Date()))()),
                ),
              ),
            ),
          ),
        )
        .limit(1)
        .for("share");
      const drain: McpRebindDrain | undefined = pending
        ? {
            reason:
              !pending.intentId || !pending.intentExpiresAt
                ? "legacy_intent"
                : pending.state === "dispatching"
                  ? "dispatch_in_progress"
                  : "unknown_intent",
            retryAfter:
              pending.intentId && pending.intentExpiresAt
                ? pending.intentExpiresAt.toISOString()
                : "",
          }
        : undefined;
      if (candidateHash !== undefined && drain) throw new McpRebindDraining(drain);
      const policies = await bounded(deps.readOperatorPolicies);
      const live = await readLiveBindingAuthority(session.profileId, policies, tx);
      if (
        !live.profilePublication ||
        live.profilePublication.grantsHash !==
          canonicalDigest(live.profileGrants.filter((grant) => grant.mcp !== undefined))
      )
        throw Error("MCP grant publication unavailable");
      const epoch = await bounded(() => deps.readBindingEpoch(sessionId));
      const applied = await bounded(() => host.readApplied(sessionId, epoch));
      let parentAuthority: { sessionId: string; snapshot: McpSessionBindingSnapshot } | undefined;
      if (owner.spawningSessionId) {
        const parent = await bounded(() => deps.readParentCurrent(owner.spawningSessionId!));
        const stored = ancestors[0]?.session;
        if (
          !stored?.mcpBindingEnvelope ||
          stored.sessionId !== owner.spawningSessionId ||
          restoreBindingSnapshot(stored.mcpBindingEnvelope, stored.integrationConnections ?? [])
            .bindingHash !== parent.bindingHash
        )
          throw Error("MCP parent authority changed");
        parentAuthority = { sessionId: owner.spawningSessionId, snapshot: parent };
      } else if (ancestors.length) throw Error("MCP parent authority changed");
      const requested = parentAuthority
        ? live.profileGrants.filter(
            (grant) =>
              grant.mcp &&
              parentAuthority!.snapshot.immutableBindings.some(
                (binding) =>
                  binding.connectionId === grant.connectionId &&
                  binding.tools.some((tool) => tool.grant.upstreamName === grant.mcp!.upstreamName),
              ),
          )
        : live.profileGrants;
      const compiled = compileSessionBindings({
        launchPolicy: { profileId: session.profileId, integrationGrants: live.profileGrants },
        liveAuthority: live,
        bindingEpoch: epoch,
        denyRevision: old.managedDenyPolicy.revision,
        requestedGrants: requested,
        parentAuthority,
      });
      const deny = prepareRebindDeny({
        sessionId,
        bindingEpoch: epoch,
        old: old.managedDenyPolicy,
        next: compiled.managedDenyPolicy,
        host: applied,
      });
      const { bindingHash: _hash, ...body } = { ...compiled, managedDenyPolicy: deny };
      const candidate = { ...body, bindingHash: canonicalDigest(body) };
      compileMcpManifest(candidate);
      if (candidateHash !== undefined) {
        if (candidate.bindingHash !== candidateHash) throw Error("stale MCP rebind candidate");
        await bounded(() =>
          applyRebindDeny(host, applied, expectedManagedDenyAck(sessionId, candidate), deny),
        );
        if ((await bounded(() => deps.readBindingEpoch(sessionId))) !== epoch)
          throw Error("MCP placement changed");
        const currentPolicies = await bounded(deps.readOperatorPolicies);
        const currentAuthority = await readLiveBindingAuthority(
          session.profileId,
          currentPolicies,
          tx,
        );
        if (managedAuthorityHash(currentAuthority) !== candidate.managedAuthorityHash)
          throw Error("MCP managed authority changed");
        const entries: IntegrationConnectionSnapshot[] = [
          ...(session.integrationConnections ?? []).filter((row) => row.provider !== "mcp"),
          ...connections.map((row) => ({
            id: row.id,
            alias: row.alias,
            provider: row.provider,
            displayName: row.displayName,
            config: row.config,
          })),
        ];
        const columns = bindingSnapshotColumns(candidate, entries);
        const updated = await tx
          .update(taskSession)
          .set(columns)
          .where(and(eq(taskSession.taskId, session.taskId), eq(taskSession.sessionId, sessionId)))
          .returning({ id: taskSession.sessionId });
        if (updated.length !== 1) throw Error("MCP session changed");
      }
      return { ...candidate, drain };
    });
  }
  return {
    prepare(sessionId: string, oldHash: string, actorId: string) {
      return run(sessionId, oldHash, actorId);
    },
    commit(sessionId: string, oldHash: string, candidateHash: string, actorId: string) {
      if (!candidateHash) throw Error("MCP reviewed candidate required");
      return run(sessionId, oldHash, actorId, candidateHash);
    },
  };
}
