import { createHash } from "node:crypto";
import { isIP } from "node:net";
import { equals } from "@bufbuild/protobuf";
import {
  ManagedPolicyTargetSchema,
  ManagedPolicyPhase,
  type ManagedAppliedState,
  type ManagedDenyPolicy as WirePolicy,
  type ManagedPolicyTarget,
} from "../gen/engram/mcp/v1/session_policy_pb.ts";
import type { SessionPolicyControlClient } from "./control-client.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import type { PublishedOperator } from "./operator.ts";
import type { ManagedDenyPolicy } from "./session-bindings.ts";
import type { ManagedDenyAck } from "./session-resolver.ts";

function number(value: bigint) {
  if (value < 0n || value > BigInt(Number.MAX_SAFE_INTEGER))
    throw Error("MCP managed integer unavailable");
  return Number(value);
}
export function managedPolicy(value: WirePolicy): ManagedDenyPolicy {
  const body = { version: 1 as const, revision: number(value.revision), hosts: [...value.hosts] };
  if (
    value.version !== 1 ||
    body.hosts.length > 4096 ||
    body.hosts.some(
      (host, i) =>
        host.length > 253 ||
        !host.includes(".") ||
        isIP(host) ||
        !host.split(".").every((label) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label)) ||
        (i > 0 && body.hosts[i - 1]! >= host),
    ) ||
    canonicalDigest(body) !== value.digest
  )
    throw Error("MCP managed policy unavailable");
  return { ...body, digest: value.digest };
}
function targetJson(target: ManagedPolicyTarget) {
  if (!target.sessionId || !target.sandboxId || !target.hostId || target.bindingEpoch <= 0n)
    throw Error("MCP managed target unavailable");
  return {
    session_id: target.sessionId,
    sandbox_id: target.sandboxId,
    host_id: target.hostId,
    binding_epoch: number(target.bindingEpoch),
  };
}
/** Exact Rust field order for a verified receipt and finalize request. */
export function managedAppliedJson(applied: ManagedAppliedState) {
  const context = applied.context,
    proof = applied.networkProof;
  if (
    !context?.target ||
    !applied.policy ||
    !applied.hostIncarnation ||
    !applied.proxyIncarnation ||
    applied.hostIncarnation.length > 128 ||
    applied.proxyIncarnation.length > 128 ||
    context.operatorGeneration <= 0n ||
    !/^sha256:[0-9a-f]{64}$/.test(context.operatorIdentity) ||
    (context.phase === ManagedPolicyPhase.FINAL
      ? !context.bindingHash || !proof
      : context.phase !== ManagedPolicyPhase.BOOTSTRAP || context.bindingHash !== undefined) ||
    (context.bindingHash !== undefined && !/^sha256:[0-9a-f]{64}$/.test(context.bindingHash))
  )
    throw Error("MCP managed receipt unavailable");
  const target = targetJson(context.target);
  if (
    proof &&
    (!proof.target ||
      !equals(ManagedPolicyTargetSchema, context.target, proof.target) ||
      isIP(proof.guestIp) !== 4 ||
      proof.guestIp === "0.0.0.0" ||
      !/^sha256:[0-9a-f]{64}$/.test(proof.networkIdentity))
  )
    throw Error("MCP network proof unavailable");
  const policy = managedPolicy(applied.policy);
  const value = {
    context: {
      target,
      operator_generation: number(context.operatorGeneration),
      operator_identity: context.operatorIdentity,
      binding_hash: context.bindingHash ?? null,
    },
    policy: {
      version: policy.version,
      revision: policy.revision,
      hosts: policy.hosts,
      digest: policy.digest,
    },
    host_incarnation: applied.hostIncarnation,
    proxy_incarnation: applied.proxyIncarnation,
    state_id: "",
    network_proof: proof
      ? {
          target: targetJson(proof.target!),
          guest_ip: proof.guestIp,
          network_identity: proof.networkIdentity,
        }
      : null,
  };
  const digest = `sha256:${createHash("sha256").update(JSON.stringify(value)).digest("hex")}`;
  if (applied.stateId !== digest) throw Error("MCP managed receipt digest changed");
  return { ...value, state_id: digest };
}
export function unionManagedPolicies(policies: readonly ManagedDenyPolicy[]): ManagedDenyPolicy {
  if (!policies.length) throw Error("MCP managed policy unavailable");
  for (const p of policies)
    managedPolicy({
      $typeName: "engram.mcp.v1.ManagedDenyPolicy",
      ...p,
      revision: BigInt(p.revision),
    });
  const revision = Math.max(...policies.map((p) => p.revision)) + 1;
  if (!Number.isSafeInteger(revision)) throw Error("MCP managed revision exhausted");
  const body = {
    version: 1 as const,
    revision,
    hosts: [...new Set(policies.flatMap((p) => p.hosts))].sort(),
  };
  return { ...body, digest: canonicalDigest(body) };
}
export function makeManagedPolicyAuthority(deps: {
  control: Pick<SessionPolicyControlClient, "readManagedPolicy" | "applyManagedPolicy">;
  readTarget(sessionId: string): Promise<ManagedPolicyTarget>;
  readPublished(): Promise<PublishedOperator>;
}) {
  async function read(target: ManagedPolicyTarget) {
    targetJson(target);
    const result = await deps.control.readManagedPolicy({ protocol: 1, target });
    if (
      result.protocol !== 1 ||
      !result.retainedDesired?.target ||
      !equals(ManagedPolicyTargetSchema, result.retainedDesired.target, target)
    )
      throw Error("MCP retained policy presence unavailable");
    const retained = result.retainedDesired.policy
      ? managedPolicy(result.retainedDesired.policy)
      : undefined;
    if (result.applied) {
      managedAppliedJson(result.applied);
      if (
        !result.applied.context?.target ||
        !equals(ManagedPolicyTargetSchema, result.applied.context.target, target)
      )
        throw Error("MCP applied target changed");
    }
    return { applied: result.applied, retained };
  }
  return {
    read,
    async enforce(expected: ManagedDenyAck, policy: ManagedDenyPolicy): Promise<ManagedDenyAck> {
      const target = await deps.readTarget(expected.sessionId);
      if (
        target.sessionId !== expected.sessionId ||
        number(target.bindingEpoch) !== expected.bindingEpoch
      )
        throw Error("MCP managed binding changed");
      const [state, publication] = await Promise.all([read(target), deps.readPublished()]);
      const applied = state.applied;
      if (
        !applied?.context ||
        !applied.policy ||
        applied.context.phase !== ManagedPolicyPhase.FINAL ||
        applied.context.bindingHash !== expected.bindingHash ||
        applied.context.operatorGeneration !== BigInt(publication.generation) ||
        applied.context.operatorIdentity !== publication.identity ||
        applied.networkProof?.networkIdentity !==
          publication.policies[0]?.publication.networkIdentity ||
        canonicalJson(managedPolicy(applied.policy)) !== canonicalJson(policy) ||
        !state.retained ||
        canonicalJson(state.retained) !== canonicalJson(policy) ||
        expected.denyDigest !== policy.digest ||
        expected.denyRevision !== policy.revision
      )
        throw Error("MCP final managed acknowledgement unavailable");
      return { ...expected };
    },
    async apply(
      target: ManagedPolicyTarget,
      bindingHash: string,
      policy: ManagedDenyPolicy,
      previous: ManagedAppliedState | undefined,
      publication: PublishedOperator,
    ) {
      const result = await deps.control.applyManagedPolicy({
        protocol: 1,
        context: {
          target,
          operatorGeneration: BigInt(publication.generation),
          operatorIdentity: publication.identity,
          phase: ManagedPolicyPhase.FINAL,
          bindingHash,
        },
        policy: { ...policy, revision: BigInt(policy.revision) },
        expected: previous,
      });
      if (result.protocol !== 1 || !result.applied) throw Error("MCP managed apply unavailable");
      managedAppliedJson(result.applied);
      const context = result.applied.context!;
      if (
        !context.target ||
        !equals(ManagedPolicyTargetSchema, context.target, target) ||
        context.bindingHash !== bindingHash ||
        context.phase !== ManagedPolicyPhase.FINAL ||
        result.applied.networkProof?.networkIdentity !==
          publication.policies[0]?.publication.networkIdentity ||
        context.operatorGeneration !== BigInt(publication.generation) ||
        context.operatorIdentity !== publication.identity ||
        canonicalJson(managedPolicy(result.applied.policy!)) !== canonicalJson(policy)
      )
        throw Error("MCP managed apply changed");
      return result.applied;
    },
  };
}
