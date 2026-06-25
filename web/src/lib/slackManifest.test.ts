import { describe, expect, test } from "vitest";
import { buildSlackManifest } from "./slackManifest";

describe("buildSlackManifest", () => {
  const origin = "https://engrams.example.com";
  const scopes = ["chat:write", "app_mentions:read", "channels:history"];
  const parse = () => JSON.parse(buildSlackManifest({ origin, scopes })) as Record<string, any>;

  test("names the app engrams (not the connector), with a bot user", () => {
    const m = parse();
    expect(m.display_information.name).toBe("engrams");
    expect(m.features.bot_user.display_name).toBe("engrams");
    expect(m.features.bot_user.always_online).toBe(true);
  });

  test("carries the OAuth redirect URL + the bot scopes verbatim", () => {
    const m = parse();
    expect(m.oauth_config.redirect_urls).toEqual([
      "https://engrams.example.com/api/v1/integrations/slack/oauth/callback",
    ]);
    expect(m.oauth_config.scopes.bot).toEqual(scopes);
  });

  test("subscribes app_mention to the events request URL (ADR 0059)", () => {
    const m = parse();
    expect(m.settings.event_subscriptions.request_url).toBe(
      "https://engrams.example.com/api/v1/integrations/slack/events",
    );
    expect(m.settings.event_subscriptions.bot_events).toContain("app_mention");
  });

  test("enables interactivity at the interactivity request URL (ADR 0059)", () => {
    const m = parse();
    expect(m.settings.interactivity.is_enabled).toBe(true);
    expect(m.settings.interactivity.request_url).toBe(
      "https://engrams.example.com/api/v1/integrations/slack/interactivity",
    );
  });

  test("disables socket mode + org deploy + token rotation (HTTP events, single workspace)", () => {
    const m = parse();
    expect(m.settings.socket_mode_enabled).toBe(false);
    expect(m.settings.org_deploy_enabled).toBe(false);
    expect(m.settings.token_rotation_enabled).toBe(false);
  });

  test("emits pretty-printed JSON (pasteable into Slack's manifest editor)", () => {
    const out = buildSlackManifest({ origin, scopes });
    expect(out).toContain("\n");
    expect(() => JSON.parse(out)).not.toThrow();
  });
});
