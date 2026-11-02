import type { McpSessionCapabilities } from "./session-capabilities.ts";
import type { makeMcpOperatorRuntime } from "./operator-runtime.ts";
import type { McpLiveAuthority, McpSessionBindingSnapshot } from "./session-bindings.ts";
import { compileBoundSessionCandidate } from "./session-create.ts";
import { assertMcpDirectConflicts } from "./session-conflicts.ts";
import { restoreBindingSnapshot } from "./binding-store.ts";
import { frozenNativeManifest } from "./managed-create-input.ts";

export function makeMcpSessionCompiler(deps: {
  operator: ReturnType<typeof makeMcpOperatorRuntime>;
  readAuthority(profileId: string): Promise<McpLiveAuthority>;
  readParentCurrent(sessionId: string): Promise<McpSessionBindingSnapshot>;
  reservedSecretReferences(): Promise<ReadonlySet<string>>;
}): McpSessionCapabilities["compile"] {
  return async (input, context) => {
    const live = await deps.readAuthority(context.profileId);
    const parent = context.spawningSessionId
      ? {
          sessionId: context.spawningSessionId,
          snapshot: await deps.readParentCurrent(context.spawningSessionId),
        }
      : undefined;
    const candidate = compileBoundSessionCandidate(input, {
      launchPolicy: {
        profileId: context.profileId,
        integrationGrants: context.profileGrants,
      },
      liveAuthority: live,
      requestedGrants: context.grants,
      bindingEpoch: 0,
      ...(parent ? { parentAuthority: parent } : {}),
    });
    const snapshot = restoreBindingSnapshot(
      candidate.mcpBindingEnvelope!,
      candidate.integrationConnections ?? [],
    );
    for (const binding of snapshot.immutableBindings)
      await deps.operator.assertCurrent(binding.config, binding.operatorPolicy);
    assertMcpDirectConflicts(
      context.directPolicy,
      snapshot.managedDenyPolicy.hosts,
      await deps.reservedSecretReferences(),
    );
    const native = frozenNativeManifest(candidate.harnessEnv?.ENGRAM_TOOLS);
    return { ...candidate, harnessEnv: { ...candidate.harnessEnv, ENGRAM_TOOLS: native.text } };
  };
}
