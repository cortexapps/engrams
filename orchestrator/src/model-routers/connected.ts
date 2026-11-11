/** Which model routers are connected.
 *
 * A router is connected when its key's org secret exists. Until an admin
 * saves that key the router is invisible to the product: no route option in
 * the composer or the profile editor, no catalog refresh, no title
 * generation or decisions through it.
 *
 * Uncached on purpose: the callers are low-frequency and the check is one
 * listing of secret names. A cache would need cross-replica invalidation to
 * avoid a stale "not connected" right after an admin saves the key.
 */

import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";
import { log as rootLog } from "../log.ts";
import { listModelRouterDefinitions, type ModelRouterDefinition } from "./registry.ts";

const log = rootLog.child({ component: "model-routers" });

export interface SecretNameClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
}

/** The connected routers, in registry order. Throws when the secret names
 * cannot be listed. */
export async function connectedRouters(
  client: SecretNameClient = defaultOrgSecret,
): Promise<ModelRouterDefinition[]> {
  const names = new Set((await client.listSecrets({})).secrets.map((secret) => secret.name));
  return listModelRouterDefinitions().filter((router) => names.has(router.credentialSecret));
}

/** The connected routers, or none when the secret names cannot be listed:
 * callers degrade, they do not retry against a key that may not exist. */
export async function connectedRoutersOrNone(
  client?: SecretNameClient,
): Promise<ModelRouterDefinition[]> {
  try {
    return await connectedRouters(client);
  } catch (err) {
    log.warn({ err }, "org-secret listing failed; treating every router as not connected");
    return [];
  }
}
