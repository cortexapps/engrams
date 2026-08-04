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
import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { IntegrationsPanel } from "./IntegrationsPanel";
import { GoogleCloudSetupWorkspace } from "../integrations/GoogleCloudSetupPage";
import {
  IntegrationService,
  type Connector,
  type IntegrationConnection,
} from "../../gen/engram/app/v1/integration_pb";
import { MintService } from "../../gen/engram/app/v1/mint_pb";
import { OrgSecretService } from "../../gen/engram/app/v1/org_secret_pb";
import { ProfileService } from "../../gen/engram/app/v1/profile_pb";

interface Caps {
  transport: ReturnType<typeof createRouterTransport>;
  puts: Array<{ name: string; value: string }>;
  upserts: string[];
  mints: Array<{ provider: string; kind: string; values: Record<string, string> }>;
  connectionCreates: Array<{
    alias: string;
    workloadIdentityProvider: string;
    serviceAccountEmail: string;
    endpoints: string[];
  }>;
  connectionUpdates: Array<{
    id: string;
    alias: string;
    workloadIdentityProvider: string;
    serviceAccountEmail: string;
    endpoints: string[];
  }>;
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
  // ADR 0109 seam: a named-connection provider comes from the catalog like any
  // other. The web used to push a hand-written Google entry in after the fact.
  {
    provider: "gcp",
    credentialSource: "mint",
    connectionModel: "named",
    hosts: ["compute.googleapis.com"],
    display: {
      name: "Google Cloud",
      category: "cloud",
      blurb: "Call Google Cloud APIs with a short-lived, policy-bound credential.",
      icon: { mono: "GC", color: "#4285f4", logo: "" },
    },
    capabilities: [
      {
        action: "compute.instances.get",
        access: "read",
        asset: "",
        label: "Describe Compute Engine instances",
        host: "compute.googleapis.com",
      },
    ],
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

function installTransport(options: { connections?: IntegrationConnection[] } = {}): Caps {
  const puts: Caps["puts"] = [];
  const upserts: string[] = [];
  const mints: Caps["mints"] = [];
  const connectionCreates: Caps["connectionCreates"] = [];
  const connectionUpdates: Caps["connectionUpdates"] = [];
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
      listConnections: () => ({ connections: options.connections ?? [] }),
      createConnection: (req) => {
        connectionCreates.push({
          alias: req.alias,
          workloadIdentityProvider: req.googleCloud?.workloadIdentityProvider ?? "",
          serviceAccountEmail: req.googleCloud?.serviceAccountEmail ?? "",
          endpoints: [...(req.googleCloud?.endpoints ?? [])],
        });
        return {
          connection: {
            id: "connection-1",
            alias: req.alias,
            provider: "gcp",
            displayName: req.displayName,
            enabled: false,
            testedAt: "",
            createdAt: "",
            updatedAt: "",
            googleCloud: req.googleCloud,
          },
        };
      },
      updateConnection: (req) => {
        connectionUpdates.push({
          id: req.id,
          alias: req.alias,
          workloadIdentityProvider: req.googleCloud?.workloadIdentityProvider ?? "",
          serviceAccountEmail: req.googleCloud?.serviceAccountEmail ?? "",
          endpoints: [...(req.googleCloud?.endpoints ?? [])],
        });
        return {
          connection: {
            id: req.id,
            alias: req.alias,
            provider: "gcp",
            displayName: req.displayName,
            enabled: false,
            testedAt: "",
            createdAt: "",
            updatedAt: "",
            googleCloud: req.googleCloud,
          },
        };
      },
      deleteConnection: () => ({ deleted: true }),
      testConnection: () => ({ ok: true, message: "STS and impersonation passed" }),
      setConnectionEnabled: () => ({ connection: undefined }),
      getGoogleCloudSetup: () => ({
        issuer: "https://engrams.example/oidc",
        audience:
          "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        subjectAttribute: "google.subject=assertion.sub",
        connectionAttribute: "attribute.engrams_connection=assertion.engrams_connection",
        gcloudScript: "gcloud iam workload-identity-pools create engrams",
        terraform: 'resource "google_iam_workload_identity_pool" "engrams" {}',
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
  return { transport, puts, upserts, mints, connectionCreates, connectionUpdates };
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
    const github = screen.getByRole("group", { name: "GitHub integration" });
    const datadog = screen.getByRole("group", { name: "Datadog integration" });
    const googleCloud = screen.getByRole("group", { name: "Google Cloud integration" });
    expect(within(github).getByRole("button", { name: /^connect$/i })).toBeTruthy();
    expect(within(datadog).getByRole("link", { name: /manage/i })).toBeTruthy();
    expect(within(googleCloud).getByRole("button", { name: /^connect$/i })).toBeTruthy();
  });

  test("Connect (mint) calls SetMintCredential with the kind + field values", async () => {
    const { transport, mints } = installTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    const github = await screen.findByRole("group", { name: "GitHub integration" });
    await user.click(within(github).getByRole("button", { name: /^connect$/i }));
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

  test("creates a keyless Google Cloud connection from familiar project inputs", async () => {
    const { transport, connectionCreates } = installTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    const googleCloud = await screen.findByRole("group", { name: "Google Cloud integration" });
    await user.click(within(googleCloud).getByRole("button", { name: /^connect$/i }));
    expect(screen.getByRole("heading", { name: "Connect Google Cloud" })).toBeTruthy();
    await user.type(screen.getByLabelText(/connection name/i), "Production read only");
    await user.type(screen.getByLabelText(/google cloud project number/i), "123");
    await user.type(
      screen.getByLabelText(/service account email/i),
      "reader@customer.iam.gserviceaccount.com",
    );
    await user.click(screen.getByRole("checkbox", { name: /cloud logging/i }));
    await user.click(screen.getByRole("checkbox", { name: /cloud trace/i }));
    await user.click(screen.getByRole("button", { name: /create and continue/i }));

    await waitFor(() => expect(connectionCreates).toHaveLength(1));
    expect(connectionCreates[0]).toEqual({
      alias: "production-read-only",
      workloadIdentityProvider:
        "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/engrams-production-read-only",
      serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
      endpoints: ["logging.googleapis.com", "cloudtrace.googleapis.com"],
    });
    expect(screen.queryByRole("heading", { name: "Connect Google Cloud" })).toBeNull();
  });

  test("shows generated Google Cloud setup in a tabbed workspace", async () => {
    const connection = {
      id: "connection-1",
      alias: "prod-readonly",
      provider: "gcp",
      displayName: "Production read only",
      enabled: false,
      testedAt: "",
      createdAt: "",
      updatedAt: "",
      googleCloud: {
        workloadIdentityProvider:
          "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
        endpoints: ["logging.googleapis.com"],
      },
    } as IntegrationConnection;
    const { transport } = installTransport({ connections: [connection] });
    const { container } = renderWithProviders(
      <GoogleCloudSetupWorkspace connectionId="connection-1" />,
      { transport },
    );
    const user = userEvent.setup();

    expect(
      await screen.findByRole("heading", { name: "Set up Production read only" }),
    ).toBeTruthy();
    expect(screen.queryByText(/gcloud iam workload-identity-pools create/)).toBeNull();
    // The FIRST highlight in the worker is expensive: shiki's JS regex engine
    // compiles the terraform grammar and tokenizes the generated config. Measured
    // ~190ms in isolation, against ~2ms for the shellscript wait below once the
    // engine is warm. `waitFor`'s 1s default leaves only ~5x headroom, and a
    // contended full-suite run eats it (this timed out on 2026-08-04, then passed
    // alone and on rerun). Pre-resolving the React.lazy chunk does NOT help —
    // measured 193ms without the warm import vs 201ms with it — so the budget,
    // not the import, is what needs fixing.
    await waitFor(
      () => {
        const terraformCode = container.querySelector(
          'code[data-language="terraform"][data-highlighted="true"]',
        );
        expect(terraformCode?.textContent).toContain("google_iam_workload_identity_pool");
      },
      { timeout: 5000 },
    );

    await user.click(screen.getByRole("tab", { name: /gcloud/i }));
    await waitFor(() => {
      const shellCode = container.querySelector(
        'code[data-language="shellscript"][data-highlighted="true"]',
      );
      expect(shellCode?.textContent).toContain("gcloud iam workload-identity-pools create");
    });
  });

  test("updates a Google Cloud connection's allowed APIs and preserves its WIF target", async () => {
    const connection = {
      id: "connection-1",
      alias: "prod-readonly",
      provider: "gcp",
      displayName: "Production read only",
      enabled: true,
      testedAt: "2026-08-02T15:01:18Z",
      createdAt: "",
      updatedAt: "",
      googleCloud: {
        workloadIdentityProvider:
          "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
        endpoints: ["logging.googleapis.com", "private-gke.example.com"],
      },
    } as IntegrationConnection;
    const { transport, connectionUpdates } = installTransport({ connections: [connection] });
    renderWithProviders(<GoogleCloudSetupWorkspace connectionId="connection-1" />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /edit apis/i }));
    expect(screen.getByRole("heading", { name: /edit allowed google cloud apis/i })).toBeTruthy();
    expect(
      (screen.getByLabelText(/other allowed api hostnames/i) as HTMLTextAreaElement).value,
    ).toBe("private-gke.example.com");
    await user.click(screen.getByRole("checkbox", { name: /cloud monitoring/i }));
    await user.click(screen.getByRole("button", { name: /save api changes/i }));

    await waitFor(() => expect(connectionUpdates).toHaveLength(1));
    expect(connectionUpdates[0]).toEqual({
      id: "connection-1",
      alias: "prod-readonly",
      workloadIdentityProvider:
        "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
      serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
      endpoints: ["logging.googleapis.com", "monitoring.googleapis.com", "private-gke.example.com"],
    });
  });

  test("shows configured Google Cloud as a connected provider card", async () => {
    const { transport } = installTransport({
      connections: [
        {
          id: "connection-1",
          alias: "prod-readonly",
          provider: "gcp",
          displayName: "Production read only",
          enabled: false,
          testedAt: "",
          createdAt: "",
          updatedAt: "",
          googleCloud: {
            workloadIdentityProvider:
              "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
            serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
            endpoints: ["logging.googleapis.com"],
          },
        } as IntegrationConnection,
      ],
    });
    renderWithProviders(<IntegrationsPanel />, { transport });

    const googleCloud = await screen.findByRole("group", { name: "Google Cloud integration" });
    expect(within(googleCloud).getByText("connected", { exact: false })).toBeTruthy();
    expect(within(googleCloud).getByText("1 connection")).toBeTruthy();
    expect(
      within(googleCloud)
        .getByRole("link", { name: /manage/i })
        .getAttribute("href"),
    ).toBe("/settings/integrations/gcp");
  });
});
