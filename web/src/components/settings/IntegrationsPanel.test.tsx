// Contract tests for the integrations marketplace (redesign).
//
//   - the catalog renders Connected vs Available provider cards (Connect /
//     Manage affordances);
//   - the Connect sheet for a mint provider calls SetMintCredential
//     (provider + kind + field values) — the orchestrator seals the org secrets;
//   - the custom-connector modal sends UpsertConnector + PutSecret for the
//     write-only credential.
// A custom router transport captures the proto-shaped requests; no fetch mocks.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { IntegrationsPanel } from "./IntegrationsPanel";
import { IntegrationService, type Connector } from "../../gen/engram/app/v1/integration_pb";
import { MintService } from "../../gen/engram/app/v1/mint_pb";
import { OrgSecretService } from "../../gen/engram/app/v1/org_secret_pb";
import { ProfileService } from "../../gen/engram/app/v1/profile_pb";

interface Caps {
  transport: ReturnType<typeof createRouterTransport>;
  puts: Array<{ name: string; value: string }>;
  upserts: string[];
  mints: Array<{ provider: string; kind: string; values: Record<string, string> }>;
}

const CATALOG = [
  {
    provider: "github",
    credentialSource: "mint",
    hosts: ["api.github.com"],
    display: {
      name: "GitHub",
      category: "Source control",
      blurb: "Read code, open pull requests.",
      icon: { mono: "GH", color: "#1f2328", logo: "" },
    },
    capabilities: [
      { action: "contents:read", access: "read", asset: "" },
      { action: "pulls:write", access: "write", asset: "pull_request" },
    ],
  },
  {
    provider: "datadog",
    credentialSource: "inject",
    hosts: ["api.datadoghq.com"],
    display: {
      name: "Datadog",
      category: "Observability",
      blurb: "Query logs and metrics.",
      icon: { mono: "DD", color: "#632ca6", logo: "" },
    },
    capabilities: [{ action: "logs:read", access: "read", asset: "query_result" }],
  },
];

// github available (needs connecting), datadog connected.
const CONNECTORS: Connector[] = [
  {
    provider: "github",
    configJson: JSON.stringify({ credential: { source: "mint", mint: { kind: "github_app" } } }),
    builtin: true,
    createdAt: "",
    updatedAt: "",
    status: "available",
  },
  {
    provider: "datadog",
    configJson: JSON.stringify({
      credential: {
        source: "inject",
        injects: [{ header: "DD-API-KEY", secretRef: "datadog-api-key", template: "{}" }],
      },
    }),
    builtin: true,
    createdAt: "",
    updatedAt: "",
    status: "connected",
  },
] as Connector[];

function installTransport(): Caps {
  const puts: Caps["puts"] = [];
  const upserts: string[] = [];
  const mints: Caps["mints"] = [];
  const transport = createRouterTransport((router) => {
    router.service(IntegrationService, {
      listConnectors: () => ({ connectors: CONNECTORS }),
      getIntegrationCatalog: () => ({ providers: CATALOG }),
      upsertConnector: (req) => {
        upserts.push(req.configJson);
        return {
          connector: {
            provider: "x",
            configJson: req.configJson,
            builtin: false,
            createdAt: "",
            updatedAt: "",
            status: "available",
          },
        };
      },
      deleteConnector: () => ({ deleted: true }),
      setMintCredential: (req) => {
        mints.push({ provider: req.provider, kind: req.kind, values: { ...req.values } });
        return { secretNames: Object.keys(req.values).map((k) => `${req.kind}.${k}`) };
      },
      uploadConnectorLogo: () => ({ logoUrl: "" }),
      testConnector: () => ({
        ok: true,
        message: "Reached api.github.com · HTTP 200 · credential accepted",
      }),
    });
    router.service(MintService, {
      listMintKinds: () => ({
        mintKinds: [
          {
            kind: "github_app",
            provider: "github",
            displayName: "GitHub App",
            fields: [
              { name: "app_id", label: "App ID", fieldKind: 1, required: true },
              { name: "private_key_pem", label: "Private key (PEM)", fieldKind: 2, required: true },
            ],
          },
        ],
      }),
    });
    router.service(OrgSecretService, {
      listSecrets: () => ({ secrets: [] }),
      putSecret: (req) => {
        puts.push({ name: req.name, value: req.value });
        return { secret: { name: req.name, keyId: "k", createdAt: "", updatedAt: "" } };
      },
      deleteSecret: () => ({ deleted: true }),
    });
    router.service(ProfileService, {
      listProfiles: () => ({ profiles: [] }),
      getProfile: () => ({ profile: undefined }),
      createProfile: () => ({ profile: undefined }),
      updateProfile: () => ({ profile: undefined }),
      deleteProfile: () => ({}),
    });
  });
  return { transport, puts, upserts, mints };
}

describe("IntegrationsPanel (marketplace)", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test("renders Connected and Available provider cards", async () => {
    const { transport } = installTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });

    await screen.findByText("GitHub");
    expect(screen.getByText("Datadog")).toBeTruthy();
    expect(screen.getByText("Connected")).toBeTruthy();
    expect(screen.getByText("Available")).toBeTruthy();
    // github is available → Connect; datadog connected → Manage.
    expect(screen.getByRole("button", { name: /^connect$/i })).toBeTruthy();
    expect(screen.getByRole("link", { name: /manage/i })).toBeTruthy();
  });

  test("Connect (mint) calls SetMintCredential with the kind + field values", async () => {
    const { transport, mints } = installTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /^connect$/i }));
    await user.type(await screen.findByLabelText(/app id/i), "1357924");
    await user.type(screen.getByLabelText(/private key/i), "-----BEGIN KEY-----");
    // Authenticate → Test → Review → Add.
    await user.click(screen.getByRole("button", { name: /continue/i }));
    await user.click(await screen.findByRole("button", { name: /test connection/i }));
    await screen.findByText(/connection verified/i);
    await user.click(screen.getByRole("button", { name: /continue/i }));
    await user.click(await screen.findByRole("button", { name: /add github/i }));

    await waitFor(() => expect(mints.length).toBe(1));
    expect(mints[0]).toEqual({
      provider: "github",
      kind: "github_app",
      values: { app_id: "1357924", private_key_pem: "-----BEGIN KEY-----" },
    });
  });

  test("Custom connector sends UpsertConnector + PutSecret for the credential", async () => {
    const { transport, puts, upserts } = installTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /custom connector/i }));
    await user.type(await screen.findByLabelText(/^provider$/i), "sentry");
    await user.type(screen.getByLabelText(/hosts/i), "sentry.io");
    await user.type(screen.getByLabelText(/org-secret name/i), "sentry-token");
    await user.type(screen.getByLabelText(/credential value/i), "sk-live-abc");
    await user.type(screen.getByLabelText("action slug"), "issues:read");
    await user.type(screen.getByLabelText("path"), "/api/0/projects/*/issues/");
    await user.click(screen.getByRole("button", { name: /add connector/i }));

    await waitFor(() => expect(upserts.length).toBe(1));
    const cfg = JSON.parse(upserts[0]!);
    expect(cfg.provider).toBe("sentry");
    expect(cfg.hosts).toEqual(["sentry.io"]);
    expect(cfg.credential.source).toBe("inject");
    expect(cfg.credential.injects[0].secretRef).toBe("sentry-token");
    expect(cfg.operations[0].grants).toEqual(["issues:read"]);
    expect(puts).toContainEqual({ name: "sentry-token", value: "sk-live-abc" });
  });
});
