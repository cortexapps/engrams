import { isIP } from "node:net";
import type { ProfileIntegrationGrant } from "../db/schema.ts";
import type { ToolManifestEntry } from "../tools/manifest.ts";
import { compileBindings, intersectMcpGrants } from "./bindings.ts";
import { reviewCatalog } from "./catalog.ts";
import { validateMcpConfig } from "./config.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import type { McpBinding, McpConnectionState, McpReviewedTool } from "./types.ts";

/** Operator-owned projection of the worker's approved zone policy. Never profile input. */
export interface McpOperatorPolicy {
  zoneId: string;
  policyRevision: string;
  policyDigest: string;
  authorities: string[];
  aliases: string[];
}
export interface McpLiveAuthority {
  profilePublication?: import("../db/profiles.ts").McpGrantPublication | null;
  profileId: string;
  profileGrants: ProfileIntegrationGrant[];
  connections: {
    id: string;
    catalogId: string;
    alias: string;
    enabled: boolean;
    config: unknown;
    state: McpConnectionState;
    tools: McpReviewedTool[];
  }[];
  operatorPolicies: McpOperatorPolicy[];
}
export interface McpSessionBinding extends McpBinding {
  catalogId: string;
  alias: string;
  operatorPolicy: McpOperatorPolicy;
}
export interface McpAncestorAuthorityRef {
  sessionId: string;
  bindingHash: string;
  bindingEpoch: number;
}
export interface ManagedDenyPolicy {
  version: 1;
  revision: number;
  hosts: string[];
  digest: string;
}
export interface McpSessionBindingSnapshot {
  version: 1;
  bindingEpoch: number;
  profileId: string;
  profileGrantsHash: string;
  profilePublication?: import("../db/profiles.ts").McpGrantPublication | null;
  managedAuthorityHash: string;
  immutableBindings: McpSessionBinding[];
  ancestorAuthorityRefs: McpAncestorAuthorityRef[];
  managedDenyPolicy: ManagedDenyPolicy;
  bindingHash: string;
}
/** Supplied only from the actual parent resolver after its full currentness check. */
export interface McpParentAuthority {
  sessionId: string;
  snapshot: McpSessionBindingSnapshot;
}

function mcpGrants(grants: readonly ProfileIntegrationGrant[]) {
  return grants.filter((grant) => grant.mcp !== undefined);
}
function profileHash(grants: readonly ProfileIntegrationGrant[]) {
  return canonicalDigest(mcpGrants(grants));
}
function normalizedPolicy(policy: McpOperatorPolicy): McpOperatorPolicy {
  if (
    !policy.zoneId ||
    !policy.policyRevision ||
    !/^sha256:[0-9a-f]{64}$/.test(policy.policyDigest)
  )
    throw new Error("operator MCP policy identity unavailable");
  const authorities = [...new Set(policy.authorities)].sort();
  const aliases = [...new Set(policy.aliases)].sort();
  for (const authority of authorities) {
    const url = new URL(`https://${authority}/`);
    if (url.host !== authority || url.username || url.password || url.pathname !== "/")
      throw new Error("operator authority must be canonical");
    assertHostname(url.hostname);
  }
  for (const alias of aliases) assertHostname(alias);
  return {
    zoneId: policy.zoneId,
    policyRevision: policy.policyRevision,
    policyDigest: policy.policyDigest,
    authorities,
    aliases,
  };
}
function assertHostname(host: string) {
  if (
    host.length > 253 ||
    isIP(host) ||
    !host.split(".").every((label) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))
  )
    throw new Error("operator hostname must be canonical");
}
function policyFor(config: ReturnType<typeof validateMcpConfig>, live: McpLiveAuthority) {
  const matches = live.operatorPolicies.filter((policy) => policy.zoneId === config.zone);
  if (matches.length !== 1) throw new Error("operator MCP zone policy unavailable");
  const policy = normalizedPolicy(matches[0]!);
  if (!policy.authorities.includes(new URL(config.endpoint).host))
    throw new Error("MCP authority is not operator approved");
  return policy;
}
export function managedAuthorityHash(live: McpLiveAuthority) {
  return canonicalDigest(
    live.connections
      .map((connection) => {
        const config = validateMcpConfig(connection.config);
        return {
          id: connection.id,
          endpoint: config.endpoint,
          operatorPolicy: policyFor(config, live),
        };
      })
      .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)),
  );
}
export function hasPublishedLeaf(snapshot: McpSessionBindingSnapshot): boolean {
  const value = snapshot.profilePublication;
  return (
    !!value &&
    Number.isSafeInteger(value.revision) &&
    value.revision >= 1 &&
    !!value.actorId &&
    value.grantsHash === snapshot.profileGrantsHash
  );
}
function effectiveGrants(snapshot: McpSessionBindingSnapshot): ProfileIntegrationGrant[] {
  return snapshot.immutableBindings.flatMap((binding) =>
    binding.tools.map((tool) => ({
      connectionId: binding.connectionId,
      operation: "invoke",
      credentialScope: "org" as const,
      resourceConstraints: [],
      mcp: structuredClone(tool.grant),
    })),
  );
}
function assertSnapshot(snapshot: McpSessionBindingSnapshot) {
  const { bindingHash, ...body } = snapshot;
  const { digest, ...deny } = snapshot.managedDenyPolicy;
  if (
    snapshot.version !== 1 ||
    canonicalDigest(body) !== bindingHash ||
    canonicalDigest(deny) !== digest
  )
    throw new Error("MCP snapshot integrity failed");
}

/** Compile a candidate only. This does not install or acknowledge guest denies. */
export function compileSessionBindings(input: {
  launchPolicy: { profileId: string; integrationGrants: readonly ProfileIntegrationGrant[] };
  liveAuthority: McpLiveAuthority;
  bindingEpoch: number;
  /** New sessions start at zero. Rebind must supply its monotonic policy revision. */
  denyRevision?: number;
  requestedGrants?: readonly ProfileIntegrationGrant[];
  parentAuthority?: McpParentAuthority;
}): McpSessionBindingSnapshot {
  const { launchPolicy, liveAuthority: live, parentAuthority: parent } = input;
  const denyRevision = input.denyRevision ?? 0;
  if (!Number.isSafeInteger(denyRevision) || denyRevision < 0)
    throw new Error("invalid managed deny revision");
  if (
    !Number.isSafeInteger(input.bindingEpoch) ||
    input.bindingEpoch < 0 ||
    launchPolicy.profileId !== live.profileId
  )
    throw new Error("MCP binding context unavailable");
  if (profileHash(launchPolicy.integrationGrants) !== profileHash(live.profileGrants))
    throw new Error("MCP profile changed during compilation");
  const publication = live.profilePublication;
  if (
    publication &&
    (!Number.isSafeInteger(publication.revision) ||
      publication.revision < 1 ||
      !publication.actorId ||
      publication.grantsHash !== profileHash(live.profileGrants))
  )
    throw new Error("MCP grant publication identity unavailable");
  let grants = mcpGrants(launchPolicy.integrationGrants);
  if (input.requestedGrants !== undefined)
    grants = intersectMcpGrants(grants, mcpGrants(input.requestedGrants));
  if (parent) {
    assertSnapshot(parent.snapshot);
    // No request means no child tools. A parent snapshot is never an implicit grant.
    grants = intersectMcpGrants(
      effectiveGrants(parent.snapshot),
      input.requestedGrants === undefined ? [] : grants,
    );
  }
  const sources = [...new Set(grants.map((grant) => grant.connectionId))].sort().map((id) => {
    const matches = live.connections.filter((connection) => connection.id === id);
    if (matches.length !== 1) throw new Error("MCP connection authority unavailable");
    const source = matches[0]!;
    if (!source.catalogId) throw new Error("MCP catalog identity unavailable");
    if (canonicalDigest(reviewCatalog(source.tools)) !== canonicalDigest(source.tools))
      throw new Error("MCP catalog review fingerprint changed");
    return { ...source, grants: grants.filter((grant) => grant.connectionId === id) };
  });
  const immutableBindings = compileBindings(sources).map((binding) => {
    const source = sources.find((source) => source.id === binding.connectionId)!;
    const extended = {
      ...binding,
      catalogId: source.catalogId,
      alias: source.alias,
      operatorPolicy: policyFor(binding.config, live),
    };
    const { hash: _hash, ...body } = extended;
    return { ...body, hash: canonicalDigest(body) };
  });
  // Keep managed destinations even when the request has no tool grants.
  const hosts = new Set<string>(parent?.snapshot.managedDenyPolicy.hosts ?? []);
  for (const connection of live.connections) {
    const config = validateMcpConfig(connection.config);
    const policy = policyFor(config, live);
    hosts.add(new URL(config.endpoint).hostname);
    for (const alias of policy.aliases) hosts.add(alias);
  }
  const deny = { version: 1 as const, revision: denyRevision, hosts: [...hosts].sort() };
  const ancestorAuthorityRefs = parent
    ? [
        {
          sessionId: parent.sessionId,
          bindingHash: parent.snapshot.bindingHash,
          bindingEpoch: parent.snapshot.bindingEpoch,
        },
        ...parent.snapshot.ancestorAuthorityRefs,
      ]
    : [];
  if (
    ancestorAuthorityRefs.length >= 32 ||
    new Set(ancestorAuthorityRefs.map((ref) => ref.sessionId)).size !== ancestorAuthorityRefs.length
  )
    throw new Error("invalid MCP ancestry");
  const body = {
    version: 1 as const,
    bindingEpoch: input.bindingEpoch,
    profileId: live.profileId,
    profileGrantsHash: profileHash(live.profileGrants),
    profilePublication: live.profilePublication ?? null,
    immutableBindings,
    ancestorAuthorityRefs,
    managedAuthorityHash: managedAuthorityHash(live),
    managedDenyPolicy: { ...deny, digest: canonicalDigest(deny) },
  };
  const snapshot = structuredClone({ ...body, bindingHash: canonicalDigest(body) });
  compileMcpManifest(snapshot);
  return snapshot;
}

/** Exact identity checks. Token bytes/expiry are deliberately absent from this authority. */
export function checkBindingCurrent(input: {
  snapshot: McpSessionBindingSnapshot;
  liveAuthority: McpLiveAuthority;
}): { current: true } | { current: false; staleReasons: string[] } {
  const staleReasons: string[] = [];
  const { snapshot, liveAuthority: live } = input;
  try {
    assertSnapshot(snapshot);
  } catch {
    staleReasons.push("snapshot_integrity");
  }
  if (
    snapshot.profileId !== live.profileId ||
    snapshot.profileGrantsHash !== profileHash(live.profileGrants) ||
    canonicalDigest(snapshot.profilePublication ?? null) !==
      canonicalDigest(live.profilePublication ?? null)
  )
    staleReasons.push("profile_grants");
  try {
    if (snapshot.managedAuthorityHash !== managedAuthorityHash(live))
      staleReasons.push("managed_destinations");
  } catch {
    staleReasons.push("managed_destinations_unavailable");
  }
  for (const binding of snapshot.immutableBindings) {
    const matches = live.connections.filter((connection) => connection.id === binding.connectionId);
    const connection = matches[0];
    if (matches.length !== 1 || !connection || !connection.enabled || connection.state.revoked) {
      staleReasons.push("connection_unavailable");
      continue;
    }
    for (const field of [
      "configRevision",
      "authGeneration",
      "credentialIdentity",
      "catalogRevision",
    ] as const)
      if (binding[field] !== connection.state[field]) staleReasons.push(field);
    if (binding.catalogId !== connection.catalogId) staleReasons.push("catalog_identity");
    if (
      binding.alias !== connection.alias ||
      canonicalDigest(binding.config) !== canonicalDigest(connection.config)
    )
      staleReasons.push("connection_identity");
    try {
      if (
        canonicalDigest(binding.operatorPolicy) !== canonicalDigest(policyFor(binding.config, live))
      )
        staleReasons.push("operator_policy");
      const reviewed = reviewCatalog(connection.tools);
      if (canonicalDigest(reviewed) !== canonicalDigest(connection.tools))
        staleReasons.push("catalog_fingerprint");
      for (const bound of binding.tools) {
        const tool = reviewed.find((tool) => tool.upstreamName === bound.tool.upstreamName);
        if (!tool?.enabled || canonicalDigest(tool) !== canonicalDigest(bound.tool))
          staleReasons.push("reviewed_tool");
      }
    } catch {
      staleReasons.push("operator_or_catalog_unavailable");
    }
  }
  return staleReasons.length
    ? { current: false, staleReasons: [...new Set(staleReasons)] }
    : { current: true };
}

/** Raw JSON Schema stays in its original dialect and resource scope. */
export function compileMcpManifest(snapshot: McpSessionBindingSnapshot): ToolManifestEntry[] {
  assertSnapshot(snapshot);
  const manifest = snapshot.immutableBindings.flatMap((binding) =>
    binding.tools.map((bound) => ({
      name: bound.name,
      description: bound.tool.description,
      inputSchema: structuredClone(bound.tool.inputSchema),
      execution: bound.tool.classification === "write" ? ("deferred" as const) : ("sync" as const),
      nativeBindings: {},
    })),
  );
  if (
    manifest.length > MCP_LIMITS.toolsPerSession ||
    Buffer.byteLength(canonicalJson(manifest)) > MCP_LIMITS.manifestBytes
  )
    throw new Error("MCP manifest exceeds session limits");
  return manifest;
}
