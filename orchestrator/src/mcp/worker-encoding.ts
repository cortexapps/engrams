import type { WorkerIdentity } from "../../../mcp-worker/src/contracts.ts";

/** The journal keeps its prefixed digest. Only the worker scope uses bare hex. */
export function toWorkerBindingHash(value: string): string {
  if (!/^sha256:[0-9a-f]{64}$/.test(value)) throw new Error("MCP binding digest invalid");
  return value.slice("sha256:".length);
}

export interface ExpectedWorkerPolicy {
  zoneId: string;
  policyRevision: string;
  /** Exact frozen operator identity, in the binding's prefixed representation. */
  policyDigest: string;
}

/** Compare representations, without changing the worker or coordinator receipt. */
export function assertWorkerPolicy(identity: WorkerIdentity, expected: ExpectedWorkerPolicy): void {
  if (
    identity.protocol !== 1 ||
    !/^[0-9a-f]{64}$/.test(identity.policyDigest) ||
    !/^sha256:[0-9a-f]{64}$/.test(expected.policyDigest) ||
    `sha256:${identity.policyDigest}` !== expected.policyDigest ||
    identity.zoneId !== expected.zoneId ||
    identity.policyRevision !== expected.policyRevision
  )
    throw new Error("MCP worker policy changed");
}
