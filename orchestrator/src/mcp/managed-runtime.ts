import { create, equals } from "@bufbuild/protobuf";
import {
  ManagedCreateIdentitySchema,
  ManagedPolicyTargetSchema,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import type { SessionPolicyControlClient } from "./control-client.ts";
import type { SessionCreateInput } from "../rpc/task-create.ts";
import type { TrustedCreationInput } from "./session-create.ts";
import type { ManagedCreationIdentity, ManagedLifecycle } from "./managed-lifecycle.ts";
import { makeManagedLifecycleStore } from "./managed-lifecycle-store.ts";
import { decodeManagedState, type ManagedCreationState } from "./managed-state.ts";
import { bytesDigest, frozenNativeManifest, managedCreationInput } from "./managed-create-input.ts";
import { bindingSnapshotColumns, restoreBindingSnapshot } from "./binding-store.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import {
  managedAppliedJson,
  makeManagedPolicyAuthority,
  unionManagedPolicies,
} from "./managed-policy.ts";
import type { McpSessionBindingSnapshot } from "./session-bindings.ts";
import type { PublishedOperator } from "./operator.ts";
import { appendMcpManifest } from "../tools/manifest.ts";
import { integrationSnapshotHash } from "../integrations/grants.ts";

type Query = {
  protocol: number;
  identity: ManagedCreationIdentity;
  nativeVersion: number;
  nativeDigest: string;
};
/** Signed identity-only producer contract. Runtime installation also requires its actual handlers. */
export interface ManagedBootBridge {
  readManagedCreationState(query: Query): Promise<{ protocol: number; stateJson: Uint8Array }>;
  prepareManagedBoot(query: Query): Promise<{ protocol: number; stateJson: Uint8Array }>;
  recoverManagedBoot(
    query: Query & { expectedSourceKeyJson: Uint8Array; recoveryId: string },
  ): Promise<{ protocol: number; stateJson: Uint8Array }>;
}
/** The immutable reservation must survive an uncertain coordinator outcome. */
export class McpManagedCreationPending extends Error {
  constructor() {
    super("MCP session creation is pending");
  }
}
export function makeManagedSessionRuntime(deps: {
  store: ReturnType<typeof makeManagedLifecycleStore>;
  control: Pick<
    SessionPolicyControlClient,
    "reserveManagedCreate" | "readNativeManifest" | "finalizeManagedBoot"
  >;
  bridge: ManagedBootBridge;
  managed: ReturnType<typeof makeManagedPolicyAuthority>;
  readPublished(): Promise<PublishedOperator>;
  validate(snapshot: McpSessionBindingSnapshot, sessionId: string): Promise<void>;
  authorize(source: TrustedCreationInput, sessionId: string): Promise<void>;
  enroll(sessionId: string): Promise<void>;
  enabled(): boolean;
}) {
  const query = (v: ManagedLifecycle): Query => ({
    protocol: 1,
    identity: v.identity,
    nativeVersion: v.nativeVersion,
    nativeDigest: v.nativeDigest,
  });
  async function read(v: ManagedLifecycle) {
    return decodeManagedState(await deps.bridge.readManagedCreationState(query(v)), v);
  }
  function publicationMatches(state: ManagedCreationState, publication: PublishedOperator) {
    const context = state.boot?.request.key.context;
    if (
      !context ||
      context.operator_generation !== publication.generation ||
      context.operator_identity !== publication.identity
    )
      throw Error("MCP boot publication changed");
  }
  async function native(v: ManagedLifecycle) {
    const result = await deps.control.readNativeManifest({ protocol: 1, identity: v.identity });
    if (
      result.protocol !== 1 ||
      !result.identity ||
      !equals(
        ManagedCreateIdentitySchema,
        result.identity,
        create(ManagedCreateIdentitySchema, v.identity),
      ) ||
      result.nativeVersion !== v.nativeVersion ||
      result.nativeDigest !== v.nativeDigest ||
      bytesDigest(result.nativeBytes) !== v.nativeDigest
    )
      throw Error("MCP frozen native manifest changed");
    return frozenNativeManifest(
      new TextDecoder("utf-8", { fatal: true }).decode(result.nativeBytes),
    ).text;
  }
  async function runOnce(sessionId: string) {
    const row = await deps.store.read(sessionId);
    let value = row.session.mcpManagedLifecycle;
    if (!value || value.phase === "terminal") return;
    let state = await read(value);
    if (state.state === "absent") return; // Cannot fence an in-flight reserve; never resend lost bytes.
    if (state.state === "terminal") {
      await deps.store.compareAndSet(value, { ...value, phase: "terminal" });
      return;
    }
    await deps.enroll(sessionId);
    if (!deps.enabled()) return;
    if (state.state === "held") {
      state = decodeManagedState(await deps.bridge.prepareManagedBoot(query(value)), value);
      if (!state.boot) return;
    }
    if (value.recovery) {
      const recovery = value.recovery;
      state = decodeManagedState(
        await deps.bridge.recoverManagedBoot({
          ...query(value),
          expectedSourceKeyJson: new TextEncoder().encode(JSON.stringify(recovery.sourceKey)),
          recoveryId: recovery.id,
        }),
        value,
      );
      if (
        state.operation?.recovery_id !== recovery.id ||
        canonicalJson(state.operation.source_key) !== canonicalJson(recovery.sourceKey)
      )
        throw Error("MCP recovery operation changed");
      if (
        !state.operation.completed ||
        !state.boot ||
        canonicalJson(state.boot.request.key) === canonicalJson(recovery.sourceKey)
      )
        return;
    }
    const boot = state.boot;
    if (!boot || ["claimed", "interrupted"].includes(boot.phase)) return;
    const key = boot.request.key;
    if (!boot.prepared) throw Error("MCP prepared receipt unavailable");
    const publication = await deps.readPublished();
    publicationMatches(state, publication);
    const target = create(ManagedPolicyTargetSchema, {
      sessionId,
      sandboxId: key.context.target.sandbox_id,
      hostId: key.context.target.host_id,
      bindingEpoch: BigInt(key.context.target.binding_epoch),
    });
    const old = restoreBindingSnapshot(
      row.session.mcpBindingEnvelope!,
      row.session.integrationConnections ?? [],
    );
    const desired = value.recovery?.candidate ?? old;
    const exactNative = await native(value);
    if (!value.pending) {
      if (boot.phase !== "prepared") throw Error("MCP final binding reservation missing");
      await deps.validate(desired, sessionId);
      const policyState = await deps.managed.read(target);
      const policies = [
        old.managedDenyPolicy,
        desired.managedDenyPolicy,
        boot.request.bootstrap_policy,
      ];
      if (policyState.retained) policies.push(policyState.retained);
      if (policyState.applied?.policy)
        policies.push({
          ...policyState.applied.policy,
          revision: Number(policyState.applied.policy.revision),
          version: 1,
        });
      const publishedHosts = [
        ...new Set(
          publication.policies.flatMap((p) => [
            ...p.authorities.map((a) => new URL(`https://${a}/`).hostname),
            ...p.aliases,
          ]),
        ),
      ].sort();
      const publishedBody = { version: 1 as const, revision: 0, hosts: publishedHosts };
      policies.push({ ...publishedBody, digest: canonicalDigest(publishedBody) });
      const { bindingHash: _hash, ...body } = desired;
      const candidate = {
        ...body,
        bindingEpoch: key.context.target.binding_epoch,
        managedDenyPolicy: unionManagedPolicies(policies),
      };
      const snapshot = { ...candidate, bindingHash: canonicalDigest(candidate) };
      const manifest = new TextEncoder().encode(
        JSON.stringify(appendMcpManifest(exactNative, snapshot)),
      );
      const next: ManagedLifecycle = {
        ...value,
        phase: "finalizing",
        sourceKey: key,
        bindingHash: snapshot.bindingHash,
        pending: {
          snapshot,
          preparedReceiptId: boot.prepared.receipt_id,
          combinedManifestDigest: bytesDigest(manifest),
        },
      };
      const columns = bindingSnapshotColumns(snapshot, [
        ...(row.session.integrationConnections ?? []).filter((c) => c.provider !== "mcp"),
        ...snapshot.immutableBindings.map((b) => ({
          id: b.connectionId,
          alias: b.alias,
          provider: "mcp",
          displayName: b.alias,
          config: b.config,
        })),
      ]);
      const integrationGrants = [
        ...(row.session.integrationGrants ?? []).filter((g) => !g.mcp),
        ...snapshot.immutableBindings.flatMap((b) =>
          b.tools.map((t) => ({
            connectionId: b.connectionId,
            operation: "invoke",
            credentialScope: "org" as const,
            resourceConstraints: [],
            mcp: t.grant,
          })),
        ),
      ];
      await deps.store.compareAndSet(value, next, {
        ...columns,
        integrationGrants,
        integrationSnapshotHash: integrationSnapshotHash({
          profileId: snapshot.profileId,
          integrationGrants,
          ...columns,
        }),
      });
      value = next;
    }
    const pending = value.pending!;
    if (
      canonicalJson(value.sourceKey) !== canonicalJson(key) ||
      pending.preparedReceiptId !== boot.prepared.receipt_id ||
      pending.snapshot.bindingEpoch !== key.context.target.binding_epoch ||
      pending.snapshot.bindingHash !== value.bindingHash
    )
      throw Error("MCP pending boot changed");
    await deps.validate(pending.snapshot, sessionId);
    const manifest = new TextEncoder().encode(
      JSON.stringify(appendMcpManifest(exactNative, pending.snapshot)),
    );
    if (bytesDigest(manifest) !== pending.combinedManifestDigest)
      throw Error("MCP final manifest changed");
    if (boot.finalization) {
      if (boot.finalization.combined_manifest_digest !== pending.combinedManifestDigest)
        throw Error("MCP finalized manifest changed");
      await deps.managed.enforce(
        {
          sessionId,
          bindingEpoch: pending.snapshot.bindingEpoch,
          bindingHash: value.bindingHash,
          denyRevision: pending.snapshot.managedDenyPolicy.revision,
          denyDigest: pending.snapshot.managedDenyPolicy.digest,
        },
        pending.snapshot.managedDenyPolicy,
      );
      if (boot.phase === "started" && value.phase !== "started")
        await deps.store.compareAndSet(value, { ...value, phase: "started" });
      return;
    }
    const policyState = await deps.managed.read(target);
    let applied = policyState.applied;
    const wanted = pending.snapshot.managedDenyPolicy;
    if (
      !applied ||
      applied.context?.bindingHash !== value.bindingHash ||
      applied.policy?.digest !== wanted.digest
    ) {
      applied = await deps.managed.apply(target, value.bindingHash, wanted, applied, publication);
    }
    // Fresh publication and binding checks immediately precede the durable finalization.
    publicationMatches(state, await deps.readPublished());
    await deps.validate(pending.snapshot, sessionId);
    const latest = (await deps.store.read(sessionId)).session.mcpManagedLifecycle;
    if (canonicalJson(latest) !== canonicalJson(value))
      throw Error("MCP finalization reservation changed");
    const response = await deps.control.finalizeManagedBoot({
      protocol: 1,
      inputJson: new TextEncoder().encode(
        JSON.stringify({
          key,
          prepared_receipt_id: pending.preparedReceiptId,
          policy: managedAppliedJson(applied),
          native_version: value.nativeVersion,
          combined_manifest: [...manifest],
          combined_manifest_digest: pending.combinedManifestDigest,
        }),
      ),
    });
    if (response.protocol !== 1 || !response.bootJson)
      throw Error("MCP finalization response unavailable");
    // A fresh read on the next pass verifies finalization and exact Started. The reply alone cannot open dispatch.
  }
  return {
    runOnce,
    read,
    native,
    async create(input: SessionCreateInput, source: TrustedCreationInput) {
      const id = input.requestedSessionId;
      if (!id || !input.mcpBindingEnvelope) throw Error("MCP creation unavailable");
      try {
        if (!deps.enabled()) throw Error("MCP creation unavailable");
        await deps.authorize(source, id);
        const row = await deps.store.read(id);
        const snapshot = restoreBindingSnapshot(
          input.mcpBindingEnvelope,
          input.integrationConnections ?? [],
        );
        await deps.validate(snapshot, id);
        const existing = row.session.mcpManagedLifecycle;
        if (existing) {
          if (
            existing.identity.actorId !== source.actorId ||
            existing.identity.inputId !== source.inputId
          )
            throw Error("MCP creation input changed");
          const state = await read(existing);
          if (state.state === "absent" || state.state === "terminal")
            throw new McpManagedCreationPending();
          await deps.enroll(id);
          return { sessionId: id };
        }
        const inputState = managedCreationInput(input, {
          ...source,
          taskId: row.session.taskId,
          ...(row.owner.spawningSessionId ? { parentSessionId: row.owner.spawningSessionId } : {}),
        });
        const value: ManagedLifecycle = {
          version: 1,
          identity: inputState.identity,
          nativeVersion: 1,
          nativeDigest: inputState.nativeDigest,
          bindingHash: snapshot.bindingHash,
          phase: "reserving",
        };
        await deps.store.initialize(value);
        try {
          const response = await deps.control.reserveManagedCreate(inputState.request);
          if (
            response.protocol !== 1 ||
            response.state !== "held" ||
            !response.identity ||
            !equals(
              ManagedCreateIdentitySchema,
              response.identity,
              create(ManagedCreateIdentitySchema, value.identity),
            )
          )
            throw Error("MCP reserve response changed");
          await deps.store.compareAndSet(value, { ...value, phase: "held" });
          await deps.enroll(id);
          return { sessionId: id };
        } catch {
          throw new McpManagedCreationPending();
        }
      } catch (error) {
        // A concurrent creator or an earlier attempt can own the durable identity.
        // Failure to read it cannot justify deleting its reservation or coordinator session.
        let retained: boolean;
        try {
          retained = Boolean((await deps.store.read(id)).session.mcpManagedLifecycle);
        } catch {
          throw new McpManagedCreationPending();
        }
        if (retained) throw new McpManagedCreationPending();
        throw error;
      }
    },
  };
}
