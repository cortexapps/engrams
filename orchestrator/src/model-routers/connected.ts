/**
 * "Is this model router connected?" — the single predicate every
 * OpenRouter-adjacent surface gates on.
 *
 * A router is connected when its credential org secret (e.g.
 * `openrouter.api_key`) exists in the org-secret store. Until an admin
 * saves that key, the router must be invisible to the product: no Route
 * option in the composer or profile editor, no catalog refresh hitting
 * the router's API, no title generation or profile picking through it —
 * a fresh deployment runs entirely on direct harness credentials.
 * (Found on the first fresh-deployment walk: a router surface that
 * assumed the key existed put the guest credential broker into an
 * infinite retry against a secret nobody had created.)
 *
 * The name listing is cached briefly: the callers here sit on hot paths
 * (task events, catalog timers) and the answer only changes when an
 * admin saves or deletes a key. Saving a key calls
 * `invalidateConnectedCache()` so the flip is visible immediately.
 */

import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";
import { getModelRouterDefinition } from "./registry.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "model-routers" });

export interface SecretNameClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
}

const CACHE_TTL_MS = 60_000;

let cache: { at: number; names: Set<string> } | null = null;

export function invalidateConnectedCache(): void {
  cache = null;
}

/** The org-secret names that currently exist, cached for CACHE_TTL_MS.
 * Fails OPEN to the empty set: if the control plane is unreachable the
 * product behaves as "no routers connected", which every caller treats
 * as a graceful degrade — never a crash loop. */
export async function configuredSecretNames(
  client: SecretNameClient = defaultOrgSecret,
): Promise<Set<string>> {
  if (cache && Date.now() - cache.at < CACHE_TTL_MS) return cache.names;
  try {
    const res = await client.listSecrets({});
    const names = new Set(res.secrets.map((secret) => secret.name));
    cache = { at: Date.now(), names };
    return names;
  } catch (err) {
    log.warn({ err }, "org-secret listing failed; treating all model routers as not connected");
    return new Set();
  }
}

/** Whether `routerId`'s credential secret exists. Unknown routers are
 * never connected. */
export async function isRouterConnected(
  routerId: string,
  client: SecretNameClient = defaultOrgSecret,
): Promise<boolean> {
  const definition = getModelRouterDefinition(routerId);
  if (!definition) return false;
  return (await configuredSecretNames(client)).has(definition.credentialSecret);
}
