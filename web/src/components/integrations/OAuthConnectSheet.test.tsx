import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { OAuthConnectSheet } from "./OAuthConnectSheet";
import type { ConnectorView } from "./useConnectorViews";
import type { ParsedOauth } from "@/lib/connectorModel";

const putSecret = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock("@/hooks/useOrgSecrets", () => ({
  usePutOrgSecret: () => ({ mutateAsync: putSecret, isPending: false }),
}));

const writeText = vi.fn().mockResolvedValue(undefined);
beforeEach(() => {
  writeText.mockClear();
  putSecret.mockClear();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});

const view = (provider = "slack"): ConnectorView => ({
  provider,
  defaultConnectionId: `connection-${provider}`,
  name: provider === "slack" ? "Slack" : "Acme",
  category: "Communication",
  blurb: "",
  icon: { mono: "SL", color: "#611f69" },
  credentialSource: "inject",
  hosts: ["slack.com"],
  capabilities: [],
  status: "available",
  builtin: true,
  usedBy: 0,
  usedByProfiles: [],
});

const oauth: ParsedOauth = {
  clientIdRef: "slack.client_id",
  clientSecretRef: "slack.client_secret",
  signingSecretRef: "slack.signing_secret",
  scopes: ["chat:write", "app_mentions:read", "channels:history"],
};

describe("OAuthConnectSheet copy buttons", () => {
  it("copies the redirect URL", async () => {
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: /copy redirect url/i }));
    await waitFor(() => expect(writeText).toHaveBeenCalled());
    expect(writeText.mock.calls[0][0]).toBe(
      `${window.location.origin}/api/v1/integrations/slack/oauth/callback`,
    );
  });

  it("copies a Slack app manifest carrying the redirect URL + scopes", async () => {
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: /copy app manifest/i }));
    await waitFor(() => expect(writeText).toHaveBeenCalled());
    const manifest = JSON.parse(writeText.mock.calls[0][0]) as Record<string, any>;
    expect(manifest.oauth_config.scopes.bot).toEqual(oauth.scopes);
    expect(manifest.oauth_config.redirect_urls[0]).toBe(
      `${window.location.origin}/api/v1/integrations/slack/oauth/callback`,
    );
    expect(manifest.settings.event_subscriptions.bot_events).toContain("app_mention");
  });

  it("offers the manifest button only for Slack", () => {
    render(<OAuthConnectSheet view={view("acme")} oauth={oauth} onClose={() => {}} />);
    expect(screen.getByRole("button", { name: /copy redirect url/i })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /copy app manifest/i })).toBeNull();
  });
});

describe("OAuthConnectSheet signing secret", () => {
  it("renders a signing-secret field when the facet declares one", () => {
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    expect(screen.getByLabelText(/signing secret/i)).toBeTruthy();
  });

  it("omits the signing-secret field when the facet has none", () => {
    render(
      <OAuthConnectSheet
        view={view()}
        oauth={{ ...oauth, signingSecretRef: undefined }}
        onClose={() => {}}
      />,
    );
    expect(screen.queryByLabelText(/signing secret/i)).toBeNull();
  });

  it("seals the signing secret under its ref on connect", async () => {
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText(/client id/i), { target: { value: "cid" } });
    fireEvent.change(screen.getByLabelText(/client secret/i), { target: { value: "csec" } });
    fireEvent.change(screen.getByLabelText(/signing secret/i), { target: { value: "sig123" } });
    fireEvent.click(screen.getByRole("button", { name: /add slack/i }));
    await waitFor(() =>
      expect(putSecret).toHaveBeenCalledWith({ name: "slack.signing_secret", value: "sig123" }),
    );
  });
});
