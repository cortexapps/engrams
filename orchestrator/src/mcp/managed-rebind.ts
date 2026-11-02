import { and, asc, eq, gt, inArray, isNull, or, sql } from "drizzle-orm";
import { create } from "@bufbuild/protobuf";
import { getDb } from "../db/client.ts";
import { integrationConnection, mcpExecution, profile, taskSession } from "../db/schema.ts";
import { ManagedPolicyTargetSchema } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { lockHumanSession } from "./journal-review.ts";
import { readLiveBindingAuthority, restoreBindingSnapshot } from "./binding-store.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import {
  compileSessionBindings,
  checkBindingCurrent,
  hasPublishedLeaf,
  type McpOperatorPolicy,
  type McpSessionBindingSnapshot,
} from "./session-bindings.ts";
import { makeManagedLifecycleStore } from "./managed-lifecycle-store.ts";
import { makeManagedSessionRuntime } from "./managed-runtime.ts";
import {
  makeManagedPolicyAuthority,
  managedPolicy,
  unionManagedPolicies,
} from "./managed-policy.ts";
import type { ManagedLifecycle } from "./managed-lifecycle.ts";
import { McpPublicationRequired, McpRebindDraining, type McpRebindDrain } from "./rebind.ts";
import { appendMcpManifest } from "../tools/manifest.ts";

/** Prepare outside locks; commit only a durable recovery intent under local authority locks. */
export function makeManagedRebind(deps: {
  db?: ReturnType<typeof getDb>;
  store: ReturnType<typeof makeManagedLifecycleStore>;
  lifecycle: ReturnType<typeof makeManagedSessionRuntime>;
  managed: ReturnType<typeof makeManagedPolicyAuthority>;
  readOperatorPolicies(): Promise<McpOperatorPolicy[]>;
  readParentCurrent(id: string): Promise<McpSessionBindingSnapshot>;
  now(): Date;
}) {
  const db = deps.db ?? getDb();
  async function drain(tx: Parameters<Parameters<typeof db.transaction>[0]>[0], sessionId: string) {
    const [pending] = await tx
      .select()
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
                gt(mcpExecution.intentExpiresAt, deps.now()),
              ),
            ),
          ),
        ),
      )
      .limit(1);
    return pending
      ? ({
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
        } satisfies McpRebindDrain)
      : undefined;
  }
  async function prepare(sessionId: string, oldHash: string, actorId: string) {
    const local = await db.transaction(async (tx) => ({
      ...(await lockHumanSession(tx, sessionId, actorId, true)),
      drain: await drain(tx, sessionId),
    }));
    const old =
      local.session.mcpBindingEnvelope &&
      restoreBindingSnapshot(
        local.session.mcpBindingEnvelope,
        local.session.integrationConnections ?? [],
      );
    const previous = local.session.mcpManagedLifecycle;
    if (
      !old ||
      old.bindingHash !== oldHash ||
      !previous?.sourceKey ||
      previous.phase !== "started" ||
      ["done", "failed", "cancelled"].includes(local.owner.status)
    )
      throw Error("MCP rebind source unavailable");
    const state = await deps.lifecycle.read(previous);
    const key = state.boot?.request.key;
    if (
      !key ||
      state.state === "terminal" ||
      canonicalJson(key) !== canonicalJson(previous.sourceKey)
    )
      throw Error("MCP rebind source changed");
    const policies = await deps.readOperatorPolicies();
    const live = await readLiveBindingAuthority(old.profileId, policies, db);
    const parentAuthority = local.owner.spawningSessionId
      ? {
          sessionId: local.owner.spawningSessionId,
          snapshot: await deps.readParentCurrent(local.owner.spawningSessionId),
        }
      : undefined;
    const next = compileSessionBindings({
      launchPolicy: { profileId: old.profileId, integrationGrants: live.profileGrants },
      liveAuthority: live,
      bindingEpoch: key.context.target.binding_epoch + 1,
      requestedGrants: live.profileGrants,
      parentAuthority,
    });
    if (!hasPublishedLeaf(next)) throw new McpPublicationRequired();
    const target = create(ManagedPolicyTargetSchema, {
      sessionId,
      sandboxId: key.context.target.sandbox_id,
      hostId: key.context.target.host_id,
      bindingEpoch: BigInt(key.context.target.binding_epoch),
    });
    const deny = [
      old.managedDenyPolicy,
      next.managedDenyPolicy,
      state.boot!.request.bootstrap_policy,
    ];
    if (state.boot!.retired) {
      // Exact durable retirement is only a deny superset, never an Applied acknowledgement.
      deny.push(state.boot!.retired.retained_policy);
    } else {
      const applied = await deps.managed.read(target);
      if (!applied.retained) throw Error("MCP retained source policy unavailable");
      deny.push(applied.retained);
      if (applied.applied?.policy) deny.push(managedPolicy(applied.applied.policy));
    }
    const { bindingHash: _hash, ...body } = next;
    const candidateBody = { ...body, managedDenyPolicy: unionManagedPolicies(deny) };
    const candidate = { ...candidateBody, bindingHash: canonicalDigest(candidateBody) };
    appendMcpManifest(await deps.lifecycle.native(previous), candidate);
    return { candidate, previous, policies, drain: local.drain };
  }
  return {
    async prepare(sessionId: string, oldHash: string, actorId: string) {
      const result = await prepare(sessionId, oldHash, actorId);
      return { ...result.candidate, ...(result.drain ? { drain: result.drain } : {}) };
    },
    async commit(sessionId: string, oldHash: string, candidateHash: string, actorId: string) {
      const current = await db.transaction(
        async (tx) =>
          (await lockHumanSession(tx, sessionId, actorId, true)).session.mcpManagedLifecycle,
      );
      if (!current) throw Error("MCP rebind source unavailable");
      if (
        !(
          current.recovery?.expectedBindingHash === oldHash &&
          current.recovery.candidateHash === candidateHash &&
          current.recovery.actorId === actorId
        )
      ) {
        const prepared = await prepare(sessionId, oldHash, actorId);
        if (prepared.candidate.bindingHash !== candidateHash)
          throw Error("stale MCP rebind candidate");
        if (prepared.drain) throw new McpRebindDraining(prepared.drain);
        const { pending: _pending, ...prior } = prepared.previous;
        const next: ManagedLifecycle = {
          ...prior,
          phase: "recovering",
          recovery: {
            id: crypto.randomUUID(),
            actorId,
            sourceKey: prior.sourceKey!,
            expectedBindingHash: oldHash,
            candidateHash,
            candidate: prepared.candidate,
          },
        };
        await db.transaction(async (tx) => {
          const local = await lockHumanSession(tx, sessionId, actorId, true, "update");
          if (
            canonicalJson(local.session.mcpManagedLifecycle) !== canonicalJson(prepared.previous) ||
            local.session.mcpBindingEnvelope?.bindingHash !== oldHash
          )
            throw Error("MCP rebind reservation changed");
          const ids = [
            ...new Set([
              prepared.candidate.profileId,
              ...local.ancestors.flatMap((a) => (a.session.profileId ? [a.session.profileId] : [])),
            ]),
          ].sort();
          await tx
            .select()
            .from(profile)
            .where(inArray(profile.id, ids))
            .orderBy(asc(profile.id))
            .for("share");
          await tx
            .select()
            .from(integrationConnection)
            .where(eq(integrationConnection.provider, "mcp"))
            .orderBy(asc(integrationConnection.id))
            .for("share");
          const live = await readLiveBindingAuthority(
            prepared.candidate.profileId,
            prepared.policies,
            tx,
          );
          if (
            !checkBindingCurrent({ snapshot: prepared.candidate, liveAuthority: live }).current ||
            prepared.candidate.ancestorAuthorityRefs.some(
              (ref, i) =>
                ref.sessionId !== local.ancestors[i]?.session.sessionId ||
                ref.bindingHash !== local.ancestors[i]?.session.mcpBindingEnvelope?.bindingHash ||
                ref.bindingEpoch !== local.ancestors[i]?.session.mcpBindingEnvelope?.bindingEpoch,
            )
          )
            throw Error("MCP rebind authority changed");
          const pending = await drain(tx, sessionId);
          if (pending) throw new McpRebindDraining(pending);
          const rows = await tx
            .update(taskSession)
            .set({ mcpManagedLifecycle: next })
            .where(
              and(
                eq(taskSession.sessionId, sessionId),
                sql`${taskSession.mcpManagedLifecycle} = ${JSON.stringify(prepared.previous)}::jsonb`,
                sql`${taskSession.mcpBindingEnvelope}->>'bindingHash' = ${oldHash}`,
              ),
            )
            .returning({ id: taskSession.sessionId });
          if (rows.length !== 1) throw Error("MCP rebind reservation changed");
        });
      }
      await deps.lifecycle.runOnce(sessionId);
      const value = (await deps.store.read(sessionId)).session.mcpManagedLifecycle;
      if (value?.phase !== "started")
        throw new McpRebindDraining({
          reason: "dispatch_in_progress",
          retryAfter: new Date(deps.now().getTime() + 1000).toISOString(),
        });
      return { bindingHash: value.bindingHash };
    },
  };
}
