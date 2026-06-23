// Contract tests for the integrations panel (ADR 0057 C4).
//
// Pins the two writes that matter:
//   - Plane A (mint): the GitHub form stores its fields as org secrets named
//     `github_app.<field>` — exactly where the coordinator resolves them (C2).
//   - Plane B (connector): the form-builder sends UpsertConnector with a config
//     whose provider it parses, plus PutSecret for the write-only credential.
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

interface Caps {
  transport: ReturnType<typeof createRouterTransport>;
  puts: Array<{ name: string; value: string }>;
  upserts: string[]; // config_json
}

function installCapturingTransport(connectors: Connector[] = []): Caps {
  const puts: Array<{ name: string; value: string }> = [];
  const upserts: string[] = [];
  const transport = createRouterTransport((router) => {
    router.service(IntegrationService, {
      listConnectors: () => ({ connectors }),
      upsertConnector: (req) => {
        upserts.push(req.configJson);
        return {
          connector: {
            provider: "x",
            configJson: req.configJson,
            builtin: false,
            createdAt: "",
            updatedAt: "",
          },
        };
      },
      deleteConnector: () => ({ deleted: true }),
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
  });
  return { transport, puts, upserts };
}

const seedConnector = (over: Partial<Connector>): Connector =>
  ({
    provider: "github",
    configJson: "{}",
    builtin: true,
    createdAt: "",
    updatedAt: "",
    ...over,
  }) as Connector;

describe("IntegrationsPanel", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test("Plane A: GitHub mint form stores fields as github_app.<field> org secrets", async () => {
    const { transport, puts } = installCapturingTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /add mint provider/i }));
    // Fields render once ListMintKinds resolves.
    const appId = await screen.findByLabelText(/app id/i);
    await user.type(appId, "12345");
    await user.type(screen.getByLabelText(/private key/i), "-----BEGIN KEY-----");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(puts.length).toBeGreaterThanOrEqual(2));
    const names = puts.map((p) => p.name);
    expect(names).toContain("github_app.app_id");
    expect(names).toContain("github_app.private_key_pem");
    expect(puts.find((p) => p.name === "github_app.app_id")?.value).toBe("12345");
  });

  test("Plane B: connector form sends UpsertConnector + PutSecret for the credential", async () => {
    const { transport, puts, upserts } = installCapturingTransport();
    renderWithProviders(<IntegrationsPanel />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /add connector/i }));
    await user.type(screen.getByLabelText(/^provider$/i), "sentry");
    await user.type(screen.getByLabelText(/^hosts$/i), "sentry.io");
    await user.type(screen.getByLabelText(/org-secret name/i), "sentry-token");
    await user.type(screen.getByLabelText(/credential value/i), "sk-live-abc");
    await user.type(screen.getByLabelText("grants"), "issues:read");
    await user.type(screen.getByLabelText("path"), "/api/0/projects/*/issues/");
    await user.click(screen.getByRole("button", { name: /save connector/i }));

    await waitFor(() => expect(upserts.length).toBeGreaterThan(0));
    const cfg = JSON.parse(upserts.at(-1)!);
    expect(cfg.provider).toBe("sentry");
    expect(cfg.hosts).toEqual(["sentry.io"]);
    expect(cfg.credential.inject.secretRef).toBe("sentry-token");
    expect(cfg.operations[0].grants).toEqual(["issues:read"]);
    // The write-only credential rides to the org secret store under the ref.
    expect(puts).toContainEqual({ name: "sentry-token", value: "sk-live-abc" });
  });

  test("built-in connectors are read-only (no Remove)", async () => {
    const { transport } = installCapturingTransport([
      seedConnector({ provider: "github", builtin: true }),
      seedConnector({
        provider: "sentry",
        builtin: false,
        configJson: '{"credential":{"source":"inject"}}',
      }),
    ]);
    renderWithProviders(<IntegrationsPanel />, { transport });

    // The github (built-in) row shows read-only; sentry (custom) has a Remove.
    await screen.findByText("github");
    expect(screen.getAllByText(/read-only/i).length).toBeGreaterThanOrEqual(1);
    expect(screen.getByRole("button", { name: /^remove$/i })).toBeTruthy();
  });
});
