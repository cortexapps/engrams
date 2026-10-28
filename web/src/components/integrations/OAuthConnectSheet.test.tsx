import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { OAuthConnectSheet } from "./OAuthConnectSheet";
import type { ConnectorView } from "./useConnectorViews";
import type { ParsedOauth } from "@/lib/connectorModel";

const putSecret = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock("@/hooks/useOrgSecrets", () => ({
  usePutOrgSecret: () => ({ mutateAsync: putSecret, isPending: false }),
}));
const grant = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock("@/hooks/useIntegrations", () => ({
  useGrantClientCredentials: () => ({ mutateAsync: grant, isPending: false }),
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
  grant: "authorization_code",
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

describe("OAuthConnectSheet client credentials", () => {
  const cc: ParsedOauth = {
    grant: "client_credentials",
    clientIdRef: "vanta.client_id",
    clientSecretRef: "vanta.client_secret",
    scopes: ["vanta-api.all:read"],
  };

  it("shows no redirect URL and runs the grant in place", async () => {
    grant.mockClear();
    const onClose = vi.fn();
    const onConnected = vi.fn();
    render(
      <OAuthConnectSheet
        view={view("vanta")}
        oauth={cc}
        onClose={onClose}
        onConnected={onConnected}
      />,
    );
    expect(screen.queryByRole("button", { name: /copy redirect url/i })).toBeNull();
    expect(screen.getByText(/Bring your own Acme API credentials/i)).toBeTruthy();
    fireEvent.change(screen.getByLabelText(/client id/i), { target: { value: "vci_1" } });
    fireEvent.change(screen.getByLabelText(/client secret/i), { target: { value: "vcs_1" } });
    fireEvent.click(screen.getByRole("button", { name: /add acme/i }));
    await waitFor(() => expect(grant).toHaveBeenCalledWith({ provider: "vanta" }));
    expect(putSecret).toHaveBeenCalledWith({ name: "vanta.client_id", value: "vci_1" });
    expect(putSecret).toHaveBeenCalledWith({ name: "vanta.client_secret", value: "vcs_1" });
    await waitFor(() => expect(onConnected).toHaveBeenCalledWith("vanta"));
    expect(onClose).toHaveBeenCalled();
  });
});

describe("OAuthConnectSheet signing secret", () => {
  it("renders a signing-secret field when the facet declares one", () => {
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    expect(screen.getByLabelText(/signing secret/i)).toBeTruthy();
  });

  it("shows the facet's own signing-secret hint, with a generic fallback", () => {
    const { unmount } = render(
      <OAuthConnectSheet
        view={view()}
        oauth={{ ...oauth, signingSecretHint: "Under App Credentials." }}
        onClose={() => {}}
      />,
    );
    expect(screen.getByText("Under App Credentials.")).toBeTruthy();
    unmount();
    render(<OAuthConnectSheet view={view()} oauth={oauth} onClose={() => {}} />);
    expect(screen.getByText(/needed only to receive Slack events/)).toBeTruthy();
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

describe("OAuthConnectSheet service account", () => {
  const sa: ParsedOauth = {
    grant: "service_account",
    serviceAccountKeyRef: "acme.service_account_key",
    clientIdRef: "acme.client_id",
    clientSecretRef: "acme.client_secret",
    scopes: ["directory.readonly"],
  };

  it("seals the key and the client, then grants as the principal", async () => {
    grant.mockClear();
    const onClose = vi.fn();
    render(<OAuthConnectSheet view={view("acme")} oauth={sa} onClose={onClose} />);
    expect(screen.queryByRole("button", { name: /copy redirect url/i })).toBeNull();
    const add = screen.getByRole("button", { name: /add acme/i });
    fireEvent.change(screen.getByLabelText(/service account key/i), {
      target: { value: '{"client_email":"sa@x","private_key":"k"}' },
    });
    fireEvent.change(screen.getByLabelText(/client id/i), { target: { value: "cid" } });
    fireEvent.change(screen.getByLabelText(/client secret/i), { target: { value: "csec" } });
    expect((add as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByLabelText(/acts as/i), { target: { value: "bot@acme.test" } });
    expect((add as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(add);
    await waitFor(() =>
      expect(grant).toHaveBeenCalledWith({ provider: "acme", subject: "bot@acme.test" }),
    );
    expect(putSecret).toHaveBeenCalledWith({
      name: "acme.service_account_key",
      value: '{"client_email":"sa@x","private_key":"k"}',
    });
    expect(putSecret).toHaveBeenCalledWith({ name: "acme.client_id", value: "cid" });
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });
});
