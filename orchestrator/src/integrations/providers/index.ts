/**
 * The named-connection provider registry (ADR 0109).
 *
 * One lookup table, built once. Every caller that used to test
 * `provider === "gcp"` asks this registry instead, so adding a second provider
 * is a new file plus one entry here — not a sweep through grant validation,
 * policy compilation, the create RPC, session create and the broker.
 *
 * The registry is cheap to build and touches neither the database nor the
 * network. That matters: grant validation and profile save consult it on every
 * request, and most of what they ask for is metadata. A provider resolves its
 * minting dependencies lazily, inside `mint`, so looking up an operation
 * catalog never opens a connection pool.
 */

import { getDb } from "../../db/client.ts";
import { makeIntegrationOidcKeyStore } from "../../db/integration-oidc-keys.ts";
import { googleOidcIssuer } from "../../routes/google-oidc.ts";
import { makeGoogleWifBroker } from "../google-wif.ts";
import { makeGoogleProvider } from "./google.ts";
import type { ConnectionProvider } from "./provider.ts";
import type { ResolvedIntegrationGrant } from "../grants.ts";
import type { IntegrationPolicyJson } from "../../connectors/registry.ts";

export type {
  ConnectionProvider,
  CredentialPurpose,
  MintIdentity,
  MintedCredential,
  ProviderConnection,
  ProviderSetupContext,
  ProviderSetupDoc,
} from "./provider.ts";

export function makeConnectionProviders(
  providers: readonly ConnectionProvider[],
): ReadonlyMap<string, ConnectionProvider> {
  const registry = new Map<string, ConnectionProvider>();
  for (const provider of providers) {
    if (registry.has(provider.key)) {
      throw new Error(`duplicate connection provider "${provider.key}"`);
    }
    registry.set(provider.key, provider);
  }
  return registry;
}

/** The Google broker, built on first MINT rather than on first lookup. */
let broker: ReturnType<typeof makeGoogleWifBroker> | undefined;
function googleBroker(): ReturnType<typeof makeGoogleWifBroker> {
  broker ??= makeGoogleWifBroker({
    keys: makeIntegrationOidcKeyStore(getDb()),
    issuer: googleOidcIssuer(),
  });
  return broker;
}

let cached: ReadonlyMap<string, ConnectionProvider> | undefined;

export function connectionProviders(): ReadonlyMap<string, ConnectionProvider> {
  cached ??= makeConnectionProviders([
    makeGoogleProvider({
      exchange: (config, identity, scopes) => googleBroker().exchange(config, identity, scopes),
    }),
  ]);
  return cached;
}

/** The provider a connection row's `provider` column names, if registered. */
export function connectionProvider(key: string): ConnectionProvider | undefined {
  return connectionProviders().get(key);
}

/**
 * Is this capability owned by a named-connection provider?
 *
 * A grant on such a connection compiles to `<provider>:<operation>`, which no
 * connector in the registry declares — the connection IS the authority, so the
 * "which connector grants this?" check does not apply. Profile save used to
 * spell this as `startsWith("gcp:")`.
 */
export function isProviderCapability(
  capability: string,
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): boolean {
  const separator = capability.indexOf(":");
  return separator > 0 && registry.has(capability.slice(0, separator));
}

/** Group resolved grants by the provider that owns their connection. */
function byProvider(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider>,
): Array<[ConnectionProvider, ResolvedIntegrationGrant[]]> {
  const grouped = new Map<string, ResolvedIntegrationGrant[]>();
  for (const entry of resolved) {
    if (!registry.has(entry.connection.provider)) continue;
    const bucket = grouped.get(entry.connection.provider);
    if (bucket) bucket.push(entry);
    else grouped.set(entry.connection.provider, [entry]);
  }
  return [...grouped].map(([key, grants]) => [registry.get(key)!, grants]);
}

/**
 * Validate grant SHAPE for every provider represented in `resolved`.
 *
 * Profile save calls this. It must stay PURE — no connection state — so that
 * disabling a connection (which editing its endpoints does automatically)
 * never blocks unrelated edits of every profile that grants it.
 */
export function validateProviderGrants(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): void {
  for (const [provider, grants] of byProvider(resolved, registry)) {
    provider.validateGrants(grants);
  }
}

/**
 * Compile every provider's grants into the session's egress policy. Called at
 * session-create, where connection STATE is finally load-bearing.
 */
export function compileProviderPolicy(
  policy: IntegrationPolicyJson,
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): void {
  for (const [provider, grants] of byProvider(resolved, registry)) {
    provider.compilePolicy(policy, grants);
  }
}

/**
 * The guest CLI surfaces the session's connections make usable, in registry
 * order so the guest manifest is stable across creates.
 */
export function providerCliSurfaces(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): Array<{ provider: string; displayName: string; bins: string[]; doc: string }> {
  const present = new Set(resolved.map((entry) => entry.connection.provider));
  return [...registry.values()]
    .filter((provider) => present.has(provider.key))
    .map((provider) => ({
      provider: provider.key,
      displayName: provider.cli.displayName,
      bins: [...provider.cli.bins],
      doc: provider.cli.doc,
    }));
}

/** Does any of these connections deliver its credential through a metadata service? */
export function providerMetadataFlavor(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): "gce" | undefined {
  for (const [provider] of byProvider(resolved, registry)) {
    if (provider.metadataFlavor) return provider.metadataFlavor;
  }
  return undefined;
}

/** Guest environment every present provider needs, merged in registry order. */
export function providerGuestEnv(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [provider] of byProvider(resolved, registry)) {
    Object.assign(env, provider.guestEnv ?? {});
  }
  return env;
}

/** Session bundles every present provider requires in the guest. */
export function providerGuestBundles(
  resolved: readonly ResolvedIntegrationGrant[],
  registry: ReadonlyMap<string, ConnectionProvider> = connectionProviders(),
): string[] {
  return byProvider(resolved, registry).flatMap(([provider]) => [...(provider.guestBundles ?? [])]);
}
