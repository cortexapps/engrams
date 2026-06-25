/**
 * Builds a Slack app manifest (ADR 0059 external triggers) the admin can paste
 * into Slack's "Create an app from a manifest" flow. It pre-fills the three things
 * that are easy to get wrong by hand: the OAuth redirect URL, the full bot scope
 * set (sourced from the connector's own `oauth.scopes`, so it never drifts from
 * what the token exchange actually requests), and the event/interactivity request
 * URLs that the orchestrator's webhook routes listen on.
 *
 * Emitted as JSON (Slack's manifest editor accepts JSON or YAML) — generated via
 * `JSON.stringify` rather than a hand-rolled serializer, so it's always valid.
 */

/** The Slack app the admin creates represents engrams in their workspace. */
const APP_NAME = "engrams";

export interface SlackManifestInput {
  /** `window.location.origin` — the public base URL the webhook routes live under. */
  origin: string;
  /** The connector's OAuth bot scopes (server-truth, from `slack.json`). */
  scopes: string[];
}

export function buildSlackManifest({ origin, scopes }: SlackManifestInput): string {
  const base = `${origin}/api/v1/integrations/slack`;
  const manifest = {
    display_information: { name: APP_NAME },
    features: {
      bot_user: { display_name: APP_NAME, always_online: true },
    },
    oauth_config: {
      redirect_urls: [`${base}/oauth/callback`],
      scopes: { bot: scopes },
    },
    settings: {
      event_subscriptions: {
        request_url: `${base}/events`,
        bot_events: ["app_mention"],
      },
      interactivity: {
        is_enabled: true,
        request_url: `${base}/interactivity`,
      },
      org_deploy_enabled: false,
      socket_mode_enabled: false,
      token_rotation_enabled: false,
    },
  };
  return JSON.stringify(manifest, null, 2);
}
