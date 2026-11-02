import type { ProfileIntegrationGrant } from "../db/schema.ts";
import { validateMcpConfig } from "./config.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import {
  validateMcpGrant,
  type McpBinding,
  type McpConnectionState,
  type McpReviewedTool,
} from "./types.ts";

export function compileBindings(
  connections: readonly {
    id: string;
    alias: string;
    config: unknown;
    state: McpConnectionState;
    enabled: boolean;
    tools: readonly McpReviewedTool[];
    grants: readonly ProfileIntegrationGrant[];
  }[],
): McpBinding[] {
  if (connections.length > MCP_LIMITS.connectionsPerSession)
    throw new Error("too many MCP connections");
  const ids = new Set<string>();
  const names = new Set<string>();
  const bindings = connections.map((connection) => {
    if (ids.has(connection.id)) throw new Error("duplicate MCP connection");
    ids.add(connection.id);
    if (
      !connection.enabled ||
      connection.state.revoked ||
      !connection.state.credentialIdentity ||
      connection.state.catalogRevision < 1
    )
      throw new Error("MCP connection is unavailable");
    if (!/^[a-z][a-z0-9-]{1,62}$/.test(connection.alias))
      throw new Error("invalid connection alias");
    const granted = new Set<string>();
    const tools = connection.grants.map((grant) => {
      validateMcpGrant(grant);
      if (grant.connectionId !== connection.id || granted.has(grant.mcp!.upstreamName))
        throw new Error("invalid or duplicate bound grant");
      granted.add(grant.mcp!.upstreamName);
      const tool = connection.tools.find((t) => t.upstreamName === grant.mcp!.upstreamName);
      if (!tool?.enabled) throw new Error("reviewed tool is unavailable");
      const name = `mcp_${connection.alias.replaceAll("-", "_").slice(0, 24)}_${canonicalDigest([connection.id, tool.upstreamName]).slice(7, 27)}`;
      if (names.has(name)) throw new Error("tool name collision");
      names.add(name);
      return { name, tool, grant: grant.mcp! };
    });
    const binding = {
      connectionId: connection.id,
      ...connection.state,
      config: validateMcpConfig(connection.config),
      tools,
      policyHash: canonicalDigest(connection.grants),
    };
    return structuredClone({ ...binding, hash: canonicalDigest(binding) });
  });
  if (
    names.size > MCP_LIMITS.toolsPerSession ||
    Buffer.byteLength(canonicalJson(bindings)) > MCP_LIMITS.manifestBytes
  )
    throw new Error("MCP manifest exceeds session limits");
  return bindings;
}

export function assertBindingCurrent(
  binding: McpBinding,
  live: McpConnectionState & { enabled: boolean },
): void {
  if (
    !live.enabled ||
    live.revoked ||
    binding.configRevision !== live.configRevision ||
    binding.authGeneration !== live.authGeneration ||
    binding.credentialIdentity !== live.credentialIdentity ||
    binding.catalogRevision !== live.catalogRevision
  )
    throw new Error("MCP binding is stale; explicit rebind is required");
}

/** Child authority can only narrow the parent's effective tool grant. */
export function intersectMcpGrants(
  parent: readonly ProfileIntegrationGrant[],
  requested: readonly ProfileIntegrationGrant[],
): ProfileIntegrationGrant[] {
  return requested.map((child) => {
    validateMcpGrant(child);
    const inherited = parent.find(
      (p) =>
        p.connectionId === child.connectionId && p.mcp?.upstreamName === child.mcp!.upstreamName,
    );
    if (!inherited) throw new Error("child tool grant exceeds parent authority");
    validateMcpGrant(inherited);
    const unrestricted = inherited.mcp!.unrestricted && child.mcp!.unrestricted;
    // Keep each root intact: allOf wrapping changes local references and resource scope.
    const argumentConstraintDocuments = [
      ...inherited.mcp!.argumentConstraintDocuments,
      ...child.mcp!.argumentConstraintDocuments,
    ];
    const effective = structuredClone({
      ...child,
      mcp: {
        ...child.mcp!,
        automationWrite: inherited.mcp!.automationWrite && child.mcp!.automationWrite,
        unrestricted,
        argumentConstraintDocuments,
      },
    });
    validateMcpGrant(effective);
    return effective;
  });
}

/** Compare only authorization data, so profile display edits do not revoke tools. */
export function assertBindingGrantsCurrent(
  binding: McpBinding,
  liveGrants: readonly ProfileIntegrationGrant[],
): void {
  const grants = liveGrants.filter((grant) => grant.connectionId === binding.connectionId);
  if (canonicalDigest(grants) !== binding.policyHash)
    throw new Error("MCP grant policy changed; explicit rebind is required");
}
