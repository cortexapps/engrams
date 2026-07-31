/** Named integration grant validation and legacy policy projection (ADR 0107). */

import { ConnectError, Code } from "@connectrpc/connect";

import type { ProfileIntegrationGrant } from "../db/schema.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";

const OPERATION_RE = /^[a-z][a-z0-9_.-]*(?::[a-z][a-z0-9_.-]*)*$/;

/** Credential-producing Google operations remain blocked at every layer. */
const FORBIDDEN_GOOGLE_OPERATIONS = new Set([
  "iam.serviceaccountkeys.create",
  "iam.generateaccesstoken",
  "iam.generateidtoken",
  "iam.signblob",
  "iam.signjwt",
]);

export interface ResolvedIntegrationGrant {
  grant: ProfileIntegrationGrant;
  connection: IntegrationConnectionRow;
}

export async function resolveIntegrationGrants(
  grants: readonly ProfileIntegrationGrant[],
  connections: IntegrationConnectionStore,
): Promise<ResolvedIntegrationGrant[]> {
  const uniqueIds = [...new Set(grants.map((grant) => grant.connectionId))];
  const rows = await Promise.all(uniqueIds.map((id) => connections.get(id)));
  const byId = new Map(
    rows.filter((row): row is IntegrationConnectionRow => row != null).map((row) => [row.id, row]),
  );

  return grants.map((grant) => {
    const connection = byId.get(grant.connectionId);
    if (!connection) {
      throw new ConnectError(
        `integration connection "${grant.connectionId}" does not exist`,
        Code.InvalidArgument,
      );
    }
    if (!OPERATION_RE.test(grant.operation)) {
      throw new ConnectError(
        `invalid integration operation "${grant.operation}"`,
        Code.InvalidArgument,
      );
    }
    if (
      connection.provider === "gcp" &&
      FORBIDDEN_GOOGLE_OPERATIONS.has(grant.operation)
    ) {
      throw new ConnectError(
        `Google Cloud operation "${grant.operation}" can produce credentials and is not allowed`,
        Code.InvalidArgument,
      );
    }
    if (grant.resourceConstraints.some((value) => value.trim() === "")) {
      throw new ConnectError("resource constraints must not be empty", Code.InvalidArgument);
    }
    return { grant, connection };
  });
}

/**
 * Project structured authority into the coordinator's current capability wire
 * format. The connection id remains in the immutable task-session snapshot and
 * is the authority for WIF token refresh; this projection only compiles egress
 * and tool policy.
 */
export function grantsToCapabilities(
  resolved: readonly ResolvedIntegrationGrant[],
): string[] {
  const capabilities = new Set<string>();
  for (const { grant, connection } of resolved) {
    if (grant.resourceConstraints.length === 0) {
      capabilities.add(`${connection.provider}:${grant.operation}`);
      continue;
    }
    for (const resource of grant.resourceConstraints) {
      capabilities.add(`${connection.provider}:${grant.operation}@${resource}`);
    }
  }
  return [...capabilities];
}

export function profileNeedsRestrictedLaunch(
  resolved: readonly ResolvedIntegrationGrant[],
): boolean {
  return resolved.some(({ connection }) => connection.provider === "gcp");
}

/** Convert a pre-ADR flat capability into its deterministic migrated grant. */
export function legacyCapabilityGrant(capability: string): ProfileIntegrationGrant {
  const at = capability.indexOf("@");
  const unscoped = at === -1 ? capability : capability.slice(0, at);
  const separator = unscoped.indexOf(":");
  if (separator <= 0 || separator === unscoped.length - 1) {
    throw new Error(`invalid legacy capability "${capability}"`);
  }
  return {
    connectionId: `legacy:${unscoped.slice(0, separator)}`,
    operation: unscoped.slice(separator + 1),
    resourceConstraints: at === -1 ? [] : [capability.slice(at + 1)],
  };
}
