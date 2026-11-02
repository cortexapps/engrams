import { isIP } from "node:net";
import { canonicalDigest } from "./hash.ts";
import type { ManagedDenyPolicy } from "./session-bindings.ts";
import { assertManagedDenyAck, type ManagedDenyAck } from "./session-resolver.ts";

/** Trusted host read must return the complete applied policy and its physical binding identity. */
export interface AppliedMcpDeny {
  sessionId: string;
  bindingEpoch: number;
  bindingHash: string;
  policy: ManagedDenyPolicy;
}
function valid(policy: ManagedDenyPolicy): boolean {
  const { digest, ...body } = policy;
  return (
    policy.version === 1 &&
    Number.isSafeInteger(policy.revision) &&
    policy.revision >= 0 &&
    Array.isArray(policy.hosts) &&
    policy.hosts.every(
      (host) =>
        typeof host === "string" &&
        host.length <= 253 &&
        !isIP(host) &&
        host.split(".").every((label) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label)),
    ) &&
    policy.hosts.every((host, index) => index === 0 || policy.hosts[index - 1]! < host) &&
    canonicalDigest(body) === digest
  );
}
/** Never drop a host-applied policy after a lost acknowledgement or failed commit. */
export function prepareRebindDeny(input: {
  sessionId: string;
  bindingEpoch: number;
  old: ManagedDenyPolicy;
  next: ManagedDenyPolicy;
  host: AppliedMcpDeny;
}): ManagedDenyPolicy {
  const { host } = input;
  if (
    !input.sessionId ||
    !Number.isSafeInteger(input.bindingEpoch) ||
    input.bindingEpoch < 0 ||
    host.sessionId !== input.sessionId ||
    host.bindingEpoch !== input.bindingEpoch ||
    !/^sha256:[a-f0-9]{64}$/.test(host.bindingHash) ||
    !valid(input.old) ||
    !valid(input.next) ||
    !valid(host.policy)
  )
    throw Error("MCP applied deny state unavailable");
  const revision = Math.max(input.old.revision, input.next.revision, host.policy.revision) + 1;
  if (!Number.isSafeInteger(revision)) throw Error("MCP deny revision exhausted");
  const body = {
    version: 1 as const,
    revision,
    hosts: [...new Set([...input.old.hosts, ...input.next.hosts, ...host.policy.hosts])].sort(),
  };
  return { ...body, digest: canonicalDigest(body) };
}

/** Task 6 supplies authenticated host operations. readApplied returns the full durable policy.
 * compareAndApply atomically checks the complete previous identity and policy, installs and
 * persists the new deny set, closes affected connections, then returns the exact ACK. It must
 * not callback into O or hold a coordinator transaction that waits for O. A timeout can
 * leave the policy installed; the next read must return that complete installed superset. */
export interface McpRebindHost {
  readApplied(sessionId: string, bindingEpoch: number): Promise<AppliedMcpDeny>;
  compareAndApply(
    previous: AppliedMcpDeny,
    expected: ManagedDenyAck,
    policy: ManagedDenyPolicy,
  ): Promise<ManagedDenyAck>;
}
export async function applyRebindDeny(
  host: Pick<McpRebindHost, "compareAndApply"> | undefined,
  previous: AppliedMcpDeny,
  expected: ManagedDenyAck,
  policy: ManagedDenyPolicy,
): Promise<void> {
  if (
    !host ||
    !valid(previous.policy) ||
    !valid(policy) ||
    expected.sessionId !== previous.sessionId ||
    expected.bindingEpoch !== previous.bindingEpoch ||
    expected.denyRevision !== policy.revision ||
    expected.denyDigest !== policy.digest ||
    policy.revision <= previous.policy.revision ||
    !previous.policy.hosts.every((value) => policy.hosts.includes(value)) ||
    !/^sha256:[a-f0-9]{64}$/.test(expected.bindingHash)
  )
    throw Error("MCP deny application unavailable");
  const ack = await host.compareAndApply(previous, expected, policy);
  assertManagedDenyAck(expected, ack);
}
