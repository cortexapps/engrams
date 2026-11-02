import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { createHash } from "node:crypto";
import { eq } from "drizzle-orm";
import { makeProfileStore } from "../db/profiles.ts";
import { task, taskSession, user } from "../db/schema.ts";
import {
  SessionPolicyService,
  ManagedPolicyPhase,
  ManagedAppliedStateSchema,
  ManagedPolicyTargetSchema,
  ManagedDenyPolicySchema,
  type ManagedAppliedState,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { bindingSnapshotColumns, readLiveBindingAuthority } from "../mcp/binding-store.ts";
import { canonicalDigest, canonicalJson } from "../mcp/hash.ts";
import { bytesDigest } from "../mcp/managed-create-input.ts";
import { makeManagedLifecycleStore } from "../mcp/managed-lifecycle-store.ts";
import type { ManagedLifecycle } from "../mcp/managed-lifecycle.ts";
import { makeManagedPolicyAuthority, managedPolicy } from "../mcp/managed-policy.ts";
import { makeManagedSessionRuntime } from "../mcp/managed-runtime.ts";
import { makeManagedRebind } from "../mcp/managed-rebind.ts";
import { managedQuery, type ManagedCreationState } from "../mcp/managed-state.ts";
import {
  checkBindingCurrent,
  compileSessionBindings,
  type McpOperatorPolicy,
  type McpSessionBindingSnapshot,
} from "../mcp/session-bindings.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";

/** Real O stores and generated client handlers; placement/capture/host are explicit producer fixtures. */
export async function coldFixture(
  existing?: Awaited<ReturnType<typeof reviewFixture>>,
  suppliedPolicies?: McpOperatorPolicy[],
  initialSnapshot?: McpSessionBindingSnapshot,
) {
  const f = existing ?? (await reviewFixture());
  const profiles = makeProfileStore(f.db);
  let currentProfile = (await profiles.get(f.profileId))!;
  if (!currentProfile.mcpGrantPublication) {
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    await profiles.update(f.profileId, currentProfile, { actorId: f.actorId, expectedRevision: 0 });
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    currentProfile = (await profiles.get(f.profileId))!;
  }
  let policies = structuredClone(suppliedPolicies ?? f.authority.operatorPolicies);
  const authority = await readLiveBindingAuthority(f.profileId, policies, f.db);
  const snapshot =
    initialSnapshot ??
    compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 1,
    });
  const [row] = await f.db.select().from(taskSession).where(eq(taskSession.sessionId, f.sessionId));
  const [owner] = await f.db.select().from(task).where(eq(task.id, f.taskId));
  const identity = {
    ...(owner?.spawningSessionId ? { parentSessionId: owner.spawningSessionId } : {}),
    sessionId: f.sessionId,
    taskId: f.taskId,
    inputId: f.sessionId,
    actorId: f.actorId,
    writeEligibility: "write_eligible",
    inputDigest: `sha256:${"a".repeat(64)}`,
  };
  const initial: ManagedLifecycle = {
    version: 1,
    nativeVersion: 1,
    nativeDigest: bytesDigest(new TextEncoder().encode("[]")),
    bindingHash: snapshot.bindingHash,
    identity,
    phase: "started",
  };
  const publication = () => ({
    generation: policies[0]!.publication.generation,
    identity: policies[0]!.publication.identity,
    policies,
  });
  function boot(
    epoch: number,
    deny: McpSessionBindingSnapshot["managedDenyPolicy"],
  ): NonNullable<ManagedCreationState["boot"]> {
    const request = {
      key: {
        creation: managedQuery(initial).identity,
        native_digest: initial.nativeDigest,
        boot_attempt: crypto.randomUUID(),
        context: {
          target: {
            session_id: f.sessionId,
            sandbox_id: crypto.randomUUID(),
            host_id: crypto.randomUUID(),
            binding_epoch: epoch,
          },
          operator_generation: publication().generation,
          operator_identity: publication().identity,
          binding_hash: null,
        },
      },
      bootstrap_policy: deny,
    };
    return {
      request,
      revision: 2,
      phase: "prepared",
      sealed_agent: null,
      finalization: null,
      retired: null,
      prepared: {
        request,
        host_incarnation: "host-incarnation",
        network_identity: policies[0]!.publication.networkIdentity,
        guest_ip: "192.0.2.7",
        receipt_id: `sha256:${"e".repeat(64)}`,
      },
    };
  }
  const state: ManagedCreationState = {
    query: managedQuery(initial),
    state: "boot",
    boot: boot(1, snapshot.managedDenyPolicy),
    operation: null,
    terminal_reason: null,
  };
  state.boot!.phase = "started";
  initial.sourceKey = state.boot!.request.key;
  await f.db
    .update(taskSession)
    .set({
      ...bindingSnapshotColumns(snapshot, row!.integrationConnections ?? []),
      mcpManagedLifecycle: initial,
    })
    .where(eq(taskSession.sessionId, f.sessionId));
  let retained = snapshot.managedDenyPolicy,
    applied: ManagedAppliedState | undefined;
  let native = "[]",
    loseRecover = false,
    loseFinalize = false;
  let recoverCalls = 0,
    applies = 0,
    finalizes = 0,
    hostReads = 0;
  const recoveryIds = new Set<string>();
  let beforeApply: () => Promise<void> = async () => {};
  let beforeRecover: () => Promise<void> = async () => {};
  let validateExtra: () => Promise<void> = async () => {};
  let parentReader: (id: string) => Promise<McpSessionBindingSnapshot> = async () => {
    throw Error("unexpected parent");
  };
  const target = () => {
    const t = state.boot!.request.key.context.target;
    return create(ManagedPolicyTargetSchema, {
      sessionId: t.session_id,
      sandboxId: t.sandbox_id,
      hostId: t.host_id,
      bindingEpoch: BigInt(t.binding_epoch),
    });
  };
  const receipt = (
    request: MessageInitShape<typeof ManagedAppliedStateSchema>["context"],
    p: MessageInitShape<typeof ManagedDenyPolicySchema>,
  ) => {
    const policy = managedPolicy(create(ManagedDenyPolicySchema, p));
    const json = {
      context: {
        target: state.boot!.request.key.context.target,
        operator_generation: publication().generation,
        operator_identity: publication().identity,
        binding_hash: request!.bindingHash ?? null,
      },
      policy,
      host_incarnation: "host-incarnation",
      proxy_incarnation: "proxy-incarnation",
      state_id: "",
      network_proof: {
        target: state.boot!.request.key.context.target,
        guest_ip: "192.0.2.7",
        network_identity: policies[0]!.publication.networkIdentity,
      },
    };
    return create(ManagedAppliedStateSchema, {
      context: request,
      policy: p,
      hostIncarnation: json.host_incarnation,
      proxyIncarnation: json.proxy_incarnation,
      stateId: `sha256:${createHash("sha256").update(JSON.stringify(json)).digest("hex")}`,
      networkProof: {
        target: target(),
        guestIp: "192.0.2.7",
        networkIdentity: policies[0]!.publication.networkIdentity,
      },
    });
  };
  applied = receipt(
    {
      target: target(),
      operatorGeneration: BigInt(publication().generation),
      operatorIdentity: publication().identity,
      phase: ManagedPolicyPhase.FINAL,
      bindingHash: snapshot.bindingHash,
    },
    { ...retained, revision: BigInt(retained.revision) },
  );
  const store = makeManagedLifecycleStore(f.db);
  const control = createClient(
    SessionPolicyService,
    createRouterTransport((router) =>
      router.service(SessionPolicyService, {
        async readNativeManifest() {
          return {
            protocol: 1,
            identity,
            nativeVersion: 1,
            nativeDigest: initial.nativeDigest,
            nativeBytes: new TextEncoder().encode(native),
          };
        },
        async readManagedPolicy() {
          hostReads++;
          return {
            protocol: 1,
            retainedDesired: {
              target: target(),
              policy: { ...retained, revision: BigInt(retained.revision) },
            },
            applied,
          };
        },
        async applyManagedPolicy(request) {
          applies++;
          await beforeApply();
          applied = receipt(request.context, request.policy!);
          retained = managedPolicy(applied.policy!);
          return { protocol: 1, applied };
        },
        async finalizeManagedBoot(request) {
          finalizes++;
          const input: {
            native_version: 1;
            combined_manifest: number[];
            combined_manifest_digest: string;
          } = JSON.parse(new TextDecoder().decode(request.inputJson));
          state.boot!.finalization = { ...input, prepared: state.boot!.prepared! };
          state.boot!.phase = "finalizing";
          if (loseFinalize) {
            loseFinalize = false;
            throw Error("lost finalize reply");
          }
          return { protocol: 1, bootJson: new TextEncoder().encode(JSON.stringify(state.boot)) };
        },
      }),
    ),
  );
  const encode = () => ({
    protocol: 1,
    stateJson: new TextEncoder().encode(JSON.stringify(state)),
  });
  const bridge = {
    readManagedCreationState: async () => encode(),
    prepareManagedBoot: async () => {
      throw Error("unexpected initial prepare");
    },
    async recoverManagedBoot(input: { recoveryId: string; expectedSourceKeyJson: Uint8Array }) {
      recoverCalls++;
      await beforeRecover();
      if (!recoveryIds.has(input.recoveryId)) {
        if (
          canonicalJson(JSON.parse(new TextDecoder().decode(input.expectedSourceKeyJson))) !==
          canonicalJson(state.boot!.request.key)
        )
          throw Error("source changed");
        recoveryIds.add(input.recoveryId);
        const source = state.boot!.request.key;
        state.boot = boot(source.context.target.binding_epoch + 1, retained);
        applied = undefined;
        state.operation = {
          intent_id: crypto.randomUUID(),
          revision: 1,
          recovery_id: input.recoveryId,
          source_key: source,
          capture_id: crypto.randomUUID(),
          completed: true,
        };
      }
      if (loseRecover) {
        loseRecover = false;
        throw Error("lost recovery reply");
      }
      return encode();
    },
  };
  const managed = makeManagedPolicyAuthority({
    control,
    readPublished: async () => publication(),
    readTarget: async () => target(),
  });
  const makeRuntime = () =>
    makeManagedSessionRuntime({
      store,
      control,
      bridge,
      managed,
      readPublished: async () => publication(),
      enabled: () => true,
      authorize: async () => {},
      enroll: async () => {},
      async validate(candidate) {
        if (
          !checkBindingCurrent({
            snapshot: candidate,
            liveAuthority: await readLiveBindingAuthority(f.profileId, policies, f.db),
          }).current
        )
          throw Error("authority changed");
        await validateExtra();
      },
    });
  const lifecycle = makeRuntime();
  let clockOffset = 0;
  const service = makeManagedRebind({
    db: f.db,
    store,
    lifecycle,
    managed,
    readOperatorPolicies: async () => policies,
    readParentCurrent: (id) => parentReader(id),
    now: () => new Date(Date.now() + clockOffset),
  });
  return {
    ...f,
    snapshot,
    store,
    lifecycle,
    service,
    profiles,
    state,
    restartRunOnce: () => makeRuntime().runOnce(f.sessionId),
    stats: () => ({ recoverCalls, recoveryIds: recoveryIds.size, applies, finalizes, hostReads }),
    advanceColdNow: (offset: number) => {
      clockOffset += offset;
    },
    loseRecover: () => {
      loseRecover = true;
    },
    loseFinalize: () => {
      loseFinalize = true;
    },
    start: () => {
      state.boot!.phase = "started";
    },
    setNative: (value: string) => {
      native = value;
    },
    beforeApply: (fn: () => Promise<void>) => {
      beforeApply = fn;
    },
    beforeRecover: (fn: () => Promise<void>) => {
      beforeRecover = fn;
    },
    validateExtra: (fn: () => Promise<void>) => {
      validateExtra = fn;
    },
    parentReader: (fn: typeof parentReader) => {
      parentReader = fn;
    },
    setPolicies: (value: McpOperatorPolicy[]) => {
      policies = value;
    },
    addRetained(host: string) {
      const body = {
        version: 1 as const,
        revision: retained.revision + 1,
        hosts: [...new Set([...retained.hosts, host])].sort(),
      };
      retained = { ...body, digest: canonicalDigest(body) };
    },
  };
}
