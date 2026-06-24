/**
 * The generic off-the-shelf-SDK seam (Mode B).
 *
 * An integration that ships a real SDK (e.g. @slack/web-api) registers a tiny
 * adapter mapping a resolved credential onto that SDK's constructor. Callers ask
 * for a ready client by provider; this resolves the credential coordinator-side
 * (./run-op.ts) and hands it to the adapter, caching the instance until the
 * credential nears expiry. Adding a new client later = one `registerIntegrationClient`
 * line (see ../integrations/slack.ts for the first one, in PR2).
 */

import { resolveIntegrationCredential, type RunOpDeps } from "./run-op.ts";
import type { ResolvedCredential } from "../gen/engram/app/v1/integration_op_pb.ts";

export type { ResolvedCredential };

/** The raw token from a single-secret / minted-bearer credential. */
export function asBearer(cred: ResolvedCredential): string {
  if (cred.cred.case === "bearer") return cred.cred.value.token;
  throw new Error(`expected a bearer credential, got "${cred.cred.case ?? "none"}"`);
}

/** The header-name → raw-value map from a multi-secret inject credential (e.g.
 * Datadog's two keys). A bearer collapses to a single Authorization header. */
export function asHeaders(cred: ResolvedCredential): Record<string, string> {
  switch (cred.cred.case) {
    case "headers":
      return cred.cred.value.values;
    case "bearer":
      return { Authorization: `Bearer ${cred.cred.value.token}` };
    default:
      throw new Error(`expected a header credential, got "${cred.cred.case ?? "none"}"`);
  }
}

/** Maps a resolved credential onto a provider's SDK client instance. */
export type IntegrationClientAdapter<T = unknown> = (cred: ResolvedCredential) => T;

const adapters = new Map<string, IntegrationClientAdapter>();

/** Register a provider's SDK adapter (idempotent; last registration wins). */
export function registerIntegrationClient<T>(provider: string, adapter: IntegrationClientAdapter<T>): void {
  adapters.set(provider, adapter as IntegrationClientAdapter);
}

interface CachedClient {
  client: unknown;
  /** epoch ms after which the cached client must be re-resolved. */
  expiresAt: number;
}
const cache = new Map<string, CachedClient>();

/** Static (no-expiry) credentials are re-resolved on this cadence so a rotation
 * is picked up without a process restart. */
const STATIC_TTL_MS = 5 * 60_000;
/** Refresh a minted/expiring credential this far ahead of its stated expiry. */
const EXPIRY_SKEW_MS = 30_000;

/**
 * A ready, authenticated SDK client for `provider`, cached until its credential
 * nears expiry. Throws if no adapter is registered for the provider.
 */
export async function getIntegrationClient<T>(provider: string, deps?: RunOpDeps): Promise<T> {
  const now = Date.now();
  const hit = cache.get(provider);
  if (hit && hit.expiresAt > now) return hit.client as T;

  const adapter = adapters.get(provider);
  if (!adapter) throw new Error(`no integration client registered for "${provider}"`);

  const cred = await resolveIntegrationCredential(provider, deps);
  const client = adapter(cred);
  const expiresAt = cred.expiresAt
    ? Date.parse(cred.expiresAt) - EXPIRY_SKEW_MS
    : now + STATIC_TTL_MS;
  cache.set(provider, { client, expiresAt });
  return client as T;
}

/** Drop a cached client (e.g. after a credential rotation or an auth failure). */
export function invalidateIntegrationClient(provider: string): void {
  cache.delete(provider);
}
