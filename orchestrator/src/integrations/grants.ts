/** Connection-aware integration grant validation and policy projection (ADR 0109). */

import { ConnectError, Code } from "@connectrpc/connect";

import type { ProfileIntegrationGrant } from "../db/schema.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";

const OPERATION_RE = /^[a-z][a-z0-9_.-]*(?::[a-z][a-z0-9_.-]*)*$/;

/** Credential-producing Google operations remain blocked at every layer. */
export const FORBIDDEN_GOOGLE_OPERATIONS = new Set([
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

/**
 * Wrap a connection store with a per-request memo so one create resolves each
 * connection id (and each provider default) at most once. Hand the SAME
 * wrapper to every resolution step of a request; a fresh wrapper per request
 * keeps rows from leaking across requests.
 */
export function withConnectionMemo(store: IntegrationConnectionStore): IntegrationConnectionStore {
  const byId = new Map<string, IntegrationConnectionRow | null>();
  const defaults = new Map<string, IntegrationConnectionRow | null>();
  return {
    ...store,
    async get(id) {
      if (!byId.has(id)) byId.set(id, await store.get(id));
      return byId.get(id)!;
    },
    async getMany(ids) {
      const missing = ids.filter((id) => !byId.has(id));
      if (missing.length > 0) {
        const rows = await store.getMany(missing);
        const found = new Map(rows.map((row) => [row.id, row]));
        for (const id of missing) byId.set(id, found.get(id) ?? null);
      }
      return ids
        .map((id) => byId.get(id))
        .filter((row): row is IntegrationConnectionRow => row != null);
    },
    async getDefault(provider) {
      if (!defaults.has(provider)) defaults.set(provider, await store.getDefault(provider));
      return defaults.get(provider)!;
    },
  };
}

export async function resolveIntegrationGrants(
  grants: readonly ProfileIntegrationGrant[],
  connections: IntegrationConnectionStore,
): Promise<ResolvedIntegrationGrant[]> {
  const uniqueIds = [...new Set(grants.map((grant) => grant.connectionId))];
  // ONE query for the whole grant set — not one per connection id.
  const rows = uniqueIds.length > 0 ? await connections.getMany(uniqueIds) : [];
  const byId = new Map(rows.map((row) => [row.id, row]));

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

interface ParsedCapabilityGrant {
  provider: string;
  operation: string;
  resourceConstraints: string[];
}

/** Parse the flat capability inputs still used by internal workflow overrides. */
function parseCapabilityGrant(capability: string): ParsedCapabilityGrant {
  const at = capability.indexOf("@");
  const unscoped = at === -1 ? capability : capability.slice(0, at);
  const separator = unscoped.indexOf(":");
  if (separator <= 0 || separator === unscoped.length - 1) {
    throw new Error(`invalid legacy capability "${capability}"`);
  }
  return {
    provider: unscoped.slice(0, separator),
    operation: unscoped.slice(separator + 1),
    resourceConstraints: at === -1 ? [] : [capability.slice(at + 1)],
  };
}

/** Bind one flat capability to an explicit connection. Test fixtures use this
 * helper too; production authority never derives a connection ID from a
 * provider name. */
export function capabilityGrant(
  capability: string,
  connectionId: string,
): ProfileIntegrationGrant {
  const parsed = parseCapabilityGrant(capability);
  return {
    connectionId,
    operation: parsed.operation,
    resourceConstraints: parsed.resourceConstraints,
  };
}

/** Resolve internal flat capability overrides through each provider's real
 * default connection. */
export async function defaultConnectionGrants(
  capabilities: readonly string[],
  connections: IntegrationConnectionStore,
): Promise<ProfileIntegrationGrant[]> {
  const parsed = capabilities.map(parseCapabilityGrant);
  const providers = [...new Set(parsed.map((capability) => capability.provider))];
  const defaults = await Promise.all(providers.map(async (provider) => {
    const connection = await connections.getDefault(provider);
    if (!connection) {
      throw new ConnectError(
        `default integration connection for "${provider}" does not exist`,
        Code.FailedPrecondition,
      );
    }
    return [provider, connection.id] as const;
  }));
  const byProvider = new Map(defaults);
  return parsed.map((capability) => ({
    connectionId: byProvider.get(capability.provider)!,
    operation: capability.operation,
    resourceConstraints: capability.resourceConstraints,
  }));
}
