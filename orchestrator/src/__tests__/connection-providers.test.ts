/**
 * The provider seam generalizes (ADR 0109).
 *
 * These tests do the thing the seam exists for: they register a SECOND
 * provider that is not Google and drive the shared code paths with it. If any
 * of those paths still assumed Google, they fail here — which is the only way
 * to know the `provider === "gcp"` branches are really gone rather than moved.
 */

import { describe, expect, test } from "bun:test";

import {
  compileProviderPolicy,
  isProviderCapability,
  makeConnectionProviders,
  providerCliSurfaces,
  providerGuestBundles,
  providerGuestEnv,
  providerMetadataFlavor,
  validateProviderGrants,
  type ConnectionProvider,
  type ProviderConnection,
} from "../integrations/providers/index.ts";
import { makeGoogleProvider } from "../integrations/providers/google.ts";
import type { ResolvedIntegrationGrant } from "../integrations/grants.ts";
import type { IntegrationPolicyJson } from "../connectors/registry.ts";

/** A minimal second provider. Nothing about it is Google-shaped. */
function makeFakeProvider(overrides: Partial<ConnectionProvider> = {}): ConnectionProvider {
  return {
    key: "acme",
    displayName: "Acme Cloud",
    blurb: "Acme APIs.",
    category: "cloud",
    cli: { displayName: "Acme", bins: ["acme"], doc: "Use the brokered token." },
    operations: {
      curated: ["acme.widgets.list"],
      passthrough: [],
      forbidden: new Set(["acme.keys.create"]),
    },
    guestEnv: { ACME_TOKEN_URL: "http://169.254.169.254/acme" },
    guestBundles: ["acme-cli"],
    validateConfig: (value) => value,
    validateGrants: (resolved) => {
      for (const { grant } of resolved) {
        if (grant.operation !== "acme.widgets.list") {
          throw new Error(`unknown Acme operation "${grant.operation}"`);
        }
      }
    },
    compilePolicy: (policy, resolved) => {
      for (const { connection } of resolved) {
        policy.injects.push({
          hosts: ["api.acme.example"],
          header_name: "",
          header_template: "",
          secret_ref: "",
          mint_source: { connection: { connection_id: connection.id, provider: "acme" } },
          methods: ["GET"],
          path_globs: ["segment-path:/v1/widgets"],
          graphql_operation: "",
          graphql_field: "",
        });
      }
    },
    mint: async () => ({ kind: "bearer", token: "acme-token", expiresAt: new Date(0) }),
    setupDoc: () => ({ audience: "acme", gcloudScript: "", terraform: "" }),
    auditIdentity: (connection) => connection.alias,
    ...overrides,
  };
}

function connection(provider: string, id: string): ProviderConnection & {
  isDefault: boolean;
  enabled: boolean;
  testedAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
} {
  return {
    id,
    alias: `${provider}-one`,
    provider,
    displayName: provider,
    config: {},
    isDefault: false,
    enabled: true,
    testedAt: new Date(0),
    createdAt: new Date(0),
    updatedAt: new Date(0),
  };
}

function grant(provider: string, operation: string): ResolvedIntegrationGrant {
  return {
    grant: { connectionId: `${provider}-1`, operation, resourceConstraints: [] },
    connection: connection(provider, `${provider}-1`) as ResolvedIntegrationGrant["connection"],
  };
}

function emptyPolicy(): IntegrationPolicyJson {
  return {
    network: { default: "deny", allow_hosts: [], allow_host_patterns: [] },
    secrets: [],
    injects: [],
    observes: [],
    google_adc: false,
  };
}

describe("connection provider seam", () => {
  const registry = makeConnectionProviders([
    makeGoogleProvider({
      exchange: async () => ({ accessToken: "ya29.x", expiresAt: new Date(0) }),
    }),
    makeFakeProvider(),
  ]);

  test("rejects a duplicate provider key", () => {
    expect(() => makeConnectionProviders([makeFakeProvider(), makeFakeProvider()])).toThrow(
      /duplicate connection provider "acme"/,
    );
  });

  test("routes each provider only its OWN grants", () => {
    // The whole point: a provider's compile step must never see another
    // provider's grants, which is what let the Google functions drop their
    // internal `provider !== "gcp"` filters.
    const seen: string[] = [];
    const spy = makeFakeProvider({
      compilePolicy: (_policy, resolved) => {
        for (const entry of resolved) seen.push(entry.connection.provider);
      },
    });
    const mixed = makeConnectionProviders([spy]);
    compileProviderPolicy(
      emptyPolicy(),
      [grant("acme", "acme.widgets.list"), grant("gcp", "logging.entries.list")],
      mixed,
    );
    expect(seen).toEqual(["acme"]);
  });

  test("compiles a non-Google provider's policy through the shared path", () => {
    const policy = emptyPolicy();
    compileProviderPolicy(policy, [grant("acme", "acme.widgets.list")], registry);
    expect(policy.injects).toHaveLength(1);
    expect(policy.injects[0]!.mint_source).toEqual({
      connection: { connection_id: "acme-1", provider: "acme" },
    });
    // The metadata endpoint is Google's delivery mechanism, not everyone's.
    expect(providerMetadataFlavor([grant("acme", "acme.widgets.list")], registry)).toBeUndefined();
    expect(providerMetadataFlavor([grant("gcp", "logging.entries.list")], registry)).toBe("gce");
  });

  test("validates grant shape per provider without touching connection state", () => {
    expect(() =>
      validateProviderGrants([grant("acme", "acme.nonsense")], registry),
    ).toThrow(/unknown Acme operation/);
    expect(() =>
      validateProviderGrants([grant("acme", "acme.widgets.list")], registry),
    ).not.toThrow();
    // A grant on an UNREGISTERED provider is skipped here rather than throwing:
    // grant resolution already rejected it, and this path must stay pure.
    expect(() => validateProviderGrants([grant("nobody", "x.y")], registry)).not.toThrow();
  });

  test("surfaces each present provider's CLI, env and bundles", () => {
    const resolved = [grant("acme", "acme.widgets.list"), grant("gcp", "logging.entries.list")];
    expect(providerCliSurfaces(resolved, registry).map((cli) => cli.provider)).toEqual([
      "gcp",
      "acme",
    ]);
    expect(providerGuestEnv(resolved, registry)).toMatchObject({
      GCE_METADATA_HOST: "169.254.169.254",
      ACME_TOKEN_URL: "http://169.254.169.254/acme",
    });
    expect(providerGuestBundles(resolved, registry).sort()).toEqual(["acme-cli", "integrations-cli"]);
    // Nothing is surfaced for a provider the session does not hold.
    expect(providerCliSurfaces([grant("acme", "acme.widgets.list")], registry)).toHaveLength(1);
  });

  test("recognizes a capability owned by any registered provider", () => {
    expect(isProviderCapability("gcp:logging.entries.list", registry)).toBe(true);
    expect(isProviderCapability("acme:acme.widgets.list", registry)).toBe(true);
    // A connector capability is authorized by the connector registry instead.
    expect(isProviderCapability("github:issues.create", registry)).toBe(false);
    expect(isProviderCapability("nocolon", registry)).toBe(false);
  });

  test("each provider names its own forbidden operations", () => {
    const acme = registry.get("acme")!;
    const google = registry.get("gcp")!;
    expect(acme.operations.forbidden.has("acme.keys.create")).toBe(true);
    expect(acme.operations.forbidden.has("iam.signjwt")).toBe(false);
    expect(google.operations.forbidden.has("iam.signjwt")).toBe(true);
  });

  test("mints through the provider and reports its own audit identity", async () => {
    const google = registry.get("gcp")!;
    const minted = await google.mint(
      {
        ...connection("gcp", "gcp-1"),
        config: {
          workloadIdentityProvider:
            "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/engrams-dev",
          serviceAccountEmail: "reader@example.iam.gserviceaccount.com",
          endpoints: ["compute.googleapis.com"],
        },
      },
      {
        sessionId: "s",
        organizationId: "o",
        connectionId: "gcp-1",
        userId: "u",
        profileSnapshotId: "sha256:x",
      },
    );
    expect(minted).toEqual({ kind: "bearer", token: "ya29.x", expiresAt: new Date(0) });
    // The broker logs whatever the provider calls an identity; it must not
    // know that Google's is a service account.
    expect(registry.get("acme")!.auditIdentity(connection("acme", "acme-1"))).toBe("acme-one");
  });
});
