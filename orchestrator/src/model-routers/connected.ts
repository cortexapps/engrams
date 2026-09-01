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
 * assumed the key existed put a consumer into an infinite retry against
 * a secret nobody had created.)
 *
 * Deliberately uncached: the callers are low-frequency (a task's first
 * prompt, a Slack routing decision, a six-hourly catalog refresh) and
 * the check is one control-plane NAME listing. A cache here would need
 * cross-replica invalidation to avoid serving a stale "not connected"
 * right after an admin saves the key — complexity the call volume does
 * not buy back.
 */

import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";
import { getModelRouterDefinition } from "./registry.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "model-routers" });

export interface SecretNameClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
}

/** Whether `routerId`'s credential secret exists. Unknown routers are
 * never connected. Fails OPEN to false: if the control plane is
 * unreachable the product behaves as "no routers connected", which
 * every caller treats as a graceful degrade — never a crash loop. */
export async function isRouterConnected(
  routerId: string,
  client: SecretNameClient = defaultOrgSecret,
): Promise<boolean> {
  const definition = getModelRouterDefinition(routerId);
  if (!definition) return false;
  try {
    const res = await client.listSecrets({});
    return res.secrets.some((secret) => secret.name === definition.credentialSecret);
  } catch (err) {
    log.warn({ err, routerId }, "org-secret listing failed; treating the router as not connected");
    return false;
  }
}
