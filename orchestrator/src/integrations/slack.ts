/**
 * Slack — adapter #1 of the generic SDK seam (clients.ts).
 *
 * The whole per-integration cost: map a resolved credential (the bot token,
 * obtained via OAuth and resolved coordinator-side) onto the official
 * `@slack/web-api` WebClient. Any orchestrator code can then do
 *   (await getSlackClient()).chat.postMessage({ channel, text })
 * — fully authed through our existing mechanism, with the expiry-aware cache from
 * clients.ts. Importing this module registers the adapter (side effect).
 */

import { WebClient } from "@slack/web-api";
import { registerIntegrationClient, asBearer, getIntegrationClient } from "./clients.ts";
import type { RunOpDeps } from "./run-op.ts";

export const SLACK_PROVIDER = "slack";

registerIntegrationClient(SLACK_PROVIDER, (cred) => new WebClient(asBearer(cred)));

/** A ready, authenticated Slack WebClient. The bot token is resolved
 * coordinator-side (Mode B) and the client is cached until it nears expiry. */
export function getSlackClient(deps?: RunOpDeps): Promise<WebClient> {
  return getIntegrationClient<WebClient>(SLACK_PROVIDER, deps);
}
