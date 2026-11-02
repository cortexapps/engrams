import type { McpBoundTool } from "./types.ts";
import {
  checkBindingCurrent,
  type McpLiveAuthority,
  type McpSessionBindingSnapshot,
  type McpSessionBinding,
  type McpAncestorAuthorityRef,
  type ManagedDenyPolicy,
} from "./session-bindings.ts";

/** Task 6 must obtain this from the trusted host after durable apply and connection closure.
 * A stored receipt, echo, or registry replacement is not an enforcement implementation. */
export interface ManagedDenyAck {
  sessionId: string;
  bindingEpoch: number;
  bindingHash: string;
  denyRevision: number;
  denyDigest: string;
}
export function expectedManagedDenyAck(
  sessionId: string,
  snapshot: McpSessionBindingSnapshot,
): ManagedDenyAck {
  return {
    sessionId,
    bindingEpoch: snapshot.bindingEpoch,
    bindingHash: snapshot.bindingHash,
    denyRevision: snapshot.managedDenyPolicy.revision,
    denyDigest: snapshot.managedDenyPolicy.digest,
  };
}
export function assertManagedDenyAck(expected: ManagedDenyAck, actual: ManagedDenyAck) {
  if (
    Object.keys(expected).some(
      (key) => expected[key as keyof ManagedDenyAck] !== actual[key as keyof ManagedDenyAck],
    )
  )
    throw new Error("MCP managed deny enforcement changed");
}
export interface BoundSessionAuthority {
  snapshot: McpSessionBindingSnapshot;
  parentSessionId: string | null;
  active: boolean;
}
export interface SessionBindingResolverDeps {
  readSession(sessionId: string): Promise<BoundSessionAuthority | null>;
  readAuthority(profileId: string): Promise<McpLiveAuthority>;
  /** Fresh coordinator policy reference. Unknown/legacy/terminal sessions must reject. */
  readBindingEpoch(sessionId: string): Promise<number>;
  /** Mandatory trusted host read/apply fence. Do not implement with a stored JSON receipt. */
  enforceManagedDeny?(expected: ManagedDenyAck, policy: ManagedDenyPolicy): Promise<ManagedDenyAck>;
}
export interface ResolvedBoundTool {
  bindingHash: string;
  binding: McpSessionBinding;
  modelToolName: string;
  reviewedTool: McpBoundTool["tool"];
  effectiveGrant: McpBoundTool["grant"];
  ancestorAuthorityRefs: McpAncestorAuthorityRef[];
  execution: "sync" | "deferred";
}
/** Session-local resolution never changes the process-wide registry or accepts a destination. */
export function makeSessionBindingResolver(deps: SessionBindingResolverDeps) {
  async function readCurrent(sessionId: string): Promise<McpSessionBindingSnapshot> {
    const leaf = await deps.readSession(sessionId);
    if (!leaf?.active) throw new Error("MCP session unavailable");
    const refs = leaf.snapshot.ancestorAuthorityRefs;
    if (refs.length >= 32) throw new Error("MCP ancestry unavailable");
    const seen = new Set<string>();
    let id: string | null = sessionId;
    for (let index = 0; index <= refs.length; index++) {
      if (!id || seen.has(id)) throw new Error("MCP ancestry unavailable");
      seen.add(id);
      const row: BoundSessionAuthority | null = index === 0 ? leaf : await deps.readSession(id);
      if (!row?.active) throw new Error("MCP ancestor unavailable");
      const snapshot = row.snapshot;
      const expected = index === 0 ? undefined : refs[index - 1];
      if (
        expected &&
        (expected.sessionId !== id ||
          expected.bindingHash !== snapshot.bindingHash ||
          expected.bindingEpoch !== snapshot.bindingEpoch)
      )
        throw new Error("MCP ancestor binding changed");
      const suffix = refs.slice(index);
      if (JSON.stringify(snapshot.ancestorAuthorityRefs) !== JSON.stringify(suffix))
        throw new Error("MCP ancestry changed");
      const live = await deps.readAuthority(snapshot.profileId);
      if (
        !checkBindingCurrent({ snapshot, liveAuthority: live }).current ||
        (await deps.readBindingEpoch(id)) !== snapshot.bindingEpoch
      )
        throw new Error("MCP authority changed");
      id = row.parentSessionId;
    }
    if (id !== null) throw new Error("MCP ancestry truncated");
    if (!deps.enforceManagedDeny) throw new Error("MCP managed deny enforcement unavailable");
    const expected = expectedManagedDenyAck(sessionId, leaf.snapshot);
    assertManagedDenyAck(
      expected,
      await deps.enforceManagedDeny(expected, leaf.snapshot.managedDenyPolicy),
    );
    return leaf.snapshot;
  }
  return {
    readCurrent,
    async resolveBoundTool(input: {
      sessionId: string;
      modelToolName: string;
    }): Promise<ResolvedBoundTool | undefined> {
      try {
        const snapshot = await readCurrent(input.sessionId);
        for (const binding of snapshot.immutableBindings) {
          const bound = binding.tools.find((tool) => tool.name === input.modelToolName);
          if (bound)
            return structuredClone({
              bindingHash: snapshot.bindingHash,
              binding,
              modelToolName: bound.name,
              reviewedTool: bound.tool,
              effectiveGrant: bound.grant,
              ancestorAuthorityRefs: snapshot.ancestorAuthorityRefs,
              execution: bound.tool.classification === "write" ? "deferred" : "sync",
            });
        }
      } catch {
        /* Missing authority, enforcement and malformed old snapshots all deny. */
      }
      return undefined;
    },
  };
}

export type SessionBindingResolver = ReturnType<typeof makeSessionBindingResolver>;
