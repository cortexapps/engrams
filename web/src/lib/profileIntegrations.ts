export interface ProfileIntegrationGrantLike {
  connectionId: string;
  operation: string;
  resourceConstraints: readonly string[];
}

/** Project migrated connector grants into the legacy policy preview format. */
export function legacyCapabilitiesForGrants(
  grants: readonly ProfileIntegrationGrantLike[],
): string[] {
  return grants.flatMap((grant) => {
    if (!grant.connectionId.startsWith("legacy:")) return [];
    const provider = grant.connectionId.slice("legacy:".length);
    return grant.resourceConstraints.length === 0
      ? [`${provider}:${grant.operation}`]
      : grant.resourceConstraints.map((resource) => `${provider}:${grant.operation}@${resource}`);
  });
}
