export interface ProfileIntegrationGrantLike {
  connectionId: string;
  operation: string;
  resourceConstraints: readonly string[];
}

export interface ConnectorViewLike {
  provider: string;
  defaultConnectionId: string;
  /** Named per-connection providers, such as Google Cloud (ADR 0109). */
  connectionModel?: "named";
  capabilities: readonly { action: string }[];
}

/**
 * Project profile grants into the flat `provider:action[@resource]` capability
 * format the policy preview consumes.
 *
 * Default-connection providers match on the grant's `connectionId` ==
 * the provider's `defaultConnectionId`. Named-connection providers (Google
 * Cloud) mint one connection ID per connection, so the catalog cannot
 * enumerate them; a grant that matches no default connection is attributed to
 * the named provider that offers its operation. Without this arm the policy
 * rail and the launch receipt showed "No powers granted" on a profile that
 * can call Compute.
 */
export function defaultCapabilitiesForGrants(
  grants: readonly ProfileIntegrationGrantLike[],
  connections: readonly ConnectorViewLike[],
): string[] {
  const providerByConnection = new Map(
    connections
      .filter((connection) => connection.defaultConnectionId !== "")
      .map((connection) => [connection.defaultConnectionId, connection.provider]),
  );
  const namedProviders = connections
    .filter((connection) => connection.connectionModel === "named")
    .map((connection) => ({
      provider: connection.provider,
      actions: new Set(connection.capabilities.map((capability) => capability.action)),
    }));
  const capabilities = grants.flatMap((grant) => {
    const provider =
      providerByConnection.get(grant.connectionId) ??
      namedProviders.find((named) => named.actions.has(grant.operation))?.provider;
    if (!provider) return [];
    return grant.resourceConstraints.length === 0
      ? [`${provider}:${grant.operation}`]
      : grant.resourceConstraints.map((resource) => `${provider}:${grant.operation}@${resource}`);
  });
  // Two named connections can grant the same operation; the preview counts
  // each capability once.
  return [...new Set(capabilities)];
}
