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
import { integrationOp as defaultIntegrationOp } from "../control-plane/client.ts";
import type { RunOpDeps } from "./run-op.ts";

export const SLACK_PROVIDER = "slack";

registerIntegrationClient(SLACK_PROVIDER, (cred) => new WebClient(asBearer(cred)));

/** A ready, authenticated Slack WebClient. The bot token is resolved
 * coordinator-side (Mode B) and the client is cached until it nears expiry. */
export function getSlackClient(deps?: RunOpDeps): Promise<WebClient> {
  return getIntegrationClient<WebClient>(SLACK_PROVIDER, deps);
}

/**
 * The Slack request-signing secret (ADR 0059) — an org secret
 * `slack.signing_secret`, set through the SAME integration settings as the bot
 * token / client creds. Resolved coordinator-side via the credential path (the
 * only orchestrator secret-read path; org secrets are otherwise write-only),
 * then cached: it's stable, and the events/interactivity webhooks verify every
 * request against it. Not a bespoke env var.
 */
export const SLACK_SIGNING_SECRET_REF = "slack.signing_secret";
const SIGNING_TTL_MS = 5 * 60_000;
let signingCache: { value: string; expiresAt: number } | undefined;

export async function getSlackSigningSecret(deps?: RunOpDeps): Promise<string> {
  const now = Date.now();
  if (signingCache && signingCache.expiresAt > now) return signingCache.value;
  const client = deps?.integrationOp ?? defaultIntegrationOp;
  const resp = await client.resolveIntegrationCredential({
    provider: SLACK_PROVIDER,
    credential: {
      source: "inject",
      mintProvider: "",
      injects: [{ header: "x-slack-signing", template: "{}", secretRef: SLACK_SIGNING_SECRET_REF }],
    },
  });
  if (!resp.credential) throw new Error("slack.signing_secret is not configured");
  const value = asBearer(resp.credential);
  signingCache = { value, expiresAt: now + SIGNING_TTL_MS };
  return value;
}

/** Test seam: drop the cached signing secret. */
export function __resetSlackSigningSecretCache(): void {
  signingCache = undefined;
}
