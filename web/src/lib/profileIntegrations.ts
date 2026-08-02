export interface ProfileIntegrationGrantLike {
  connectionId: string;
  operation: string;
  resourceConstraints: readonly string[];
}

export interface DefaultConnectionLike {
  provider: string;
  defaultConnectionId: string;
}

/** Project grants on existing providers' default connections into the flat
 * capability format consumed by the policy preview. Named provider-specific
 * connections, such as Google Cloud, are handled by their own policy view. */
export function defaultCapabilitiesForGrants(
  grants: readonly ProfileIntegrationGrantLike[],
  connections: readonly DefaultConnectionLike[],
): string[] {
  const providerByConnection = new Map(
    connections.map((connection) => [connection.defaultConnectionId, connection.provider]),
  );
  return grants.flatMap((grant) => {
    const provider = providerByConnection.get(grant.connectionId);
    if (!provider) return [];
    return grant.resourceConstraints.length === 0
      ? [`${provider}:${grant.operation}`]
      : grant.resourceConstraints.map((resource) => `${provider}:${grant.operation}@${resource}`);
  });
}
