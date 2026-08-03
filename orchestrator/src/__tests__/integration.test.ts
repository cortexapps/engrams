/**
 * Native IntegrationService tests (ADR 0057 C3) — connector catalog CRUD.
 *
 *   - all RPCs admin-only (401 anon, 403 member)
 *   - ListConnectors merges read-only built-in seeds (github/datadog) with custom
 *   - UpsertConnector validates via parseConnector + rejects built-in providers
 *   - DeleteConnector rejects built-in providers; deletes custom
 */

import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerIntegration } from "../rpc/integration.ts";
import type { IntegrationDeps, GetSession, MintAccess, OrgSecretAccess } from "../rpc/integration.ts";
import type { ConnectorStore, ConnectorRow } from "../db/connectors.ts";
import type { ConnectorLogoStore } from "../db/connector-logos.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import type { ProfileStore } from "../db/profiles.ts";
import { invalidateRegistry } from "../connectors/registry.ts";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import type { MintKind } from "../gen/engram/app/v1/mint_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

// The github_app mint kind (shape mirrors mint.test.ts; only name/required are read).
const GITHUB_MINT_KIND = {
  kind: "github_app",
  provider: "github",
  displayName: "GitHub App",
  fields: [
    { name: "app_id", label: "App ID", fieldKind: 1, required: true },
    { name: "private_key_pem", label: "Private key (PEM)", fieldKind: 2, required: true },
  ],
} as unknown as MintKind;

function fakeMint(onTest?: (spec: unknown) => { ok: boolean; message: string }): MintAccess {
  return {
    async listMintKinds() {
      return { mintKinds: [GITHUB_MINT_KIND] };
    },
    async runConnectorTest(spec) {
      return onTest?.(spec) ?? { ok: true, message: "ok" };
    },
  };
}

function fakeOrgSecret(names: string[] = []) {
  const set = new Set(names);
  const puts: Array<{ name: string; value: string }> = [];
  const client: OrgSecretAccess = {
    async listSecrets() {
      return { secrets: [...set].map((name) => ({ name })) };
    },
    async putSecret(req) {
      puts.push(req);
      set.add(req.name);
      return {};
    },
  };
  return { client, puts };
}

function fakeLogoStore(seed: Record<string, { mediaType: string; data: Buffer }> = {}) {
  const rows = new Map(Object.entries(seed));
  const store: ConnectorLogoStore = {
    async get(provider) {
      const r = rows.get(provider);
      return r ? { provider, mediaType: r.mediaType, data: r.data, updatedAt: new Date(0) } : null;
    },
    async put(provider, mediaType, data) {
      rows.set(provider, { mediaType, data });
    },
    async delete(provider) {
      return rows.delete(provider);
    },
    async listProviders() {
      return [...rows.keys()];
    },
  };
  return { store, rows };
}

// Minimal valid magic-byte fixtures for the logo sniff.
const PNG_BYTES = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
const SVG_BYTES = new TextEncoder().encode('<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>');

interface Recorded {
  upserts: Array<{ provider: string; config: unknown }>;
  deletes: string[];
}

function fakeStore(seed: ConnectorRow[] = []): { store: ConnectorStore; rec: Recorded } {
  const rec: Recorded = { upserts: [], deletes: [] };
  const rows = new Map(seed.map((r) => [r.provider, r]));
  const store: ConnectorStore = {
    async list() {
      return [...rows.values()];
    },
    async get(provider) {
      return rows.get(provider) ?? null;
    },
    async upsert(provider, config) {
      rec.upserts.push({ provider, config });
      const row: ConnectorRow = {
        provider,
        config,
        createdAt: new Date("2026-01-01T00:00:00Z"),
        updatedAt: new Date("2026-01-01T00:00:00Z"),
      };
      rows.set(provider, row);
      return row;
    },
    async delete(provider) {
      rec.deletes.push(provider);
      return rows.delete(provider);
    },
  };
  return { store, rec };
}

function fakeConnectionStore(seed: IntegrationConnectionRow[] = []): IntegrationConnectionStore {
  const rows = new Map(seed.map((row) => [row.id, row]));
  let sequence = rows.size;
  return {
    async list() {
      return [...rows.values()];
    },
    async get(id) {
      return rows.get(id) ?? null;
    },
    async getMany(ids) {
      return ids.map((id) => rows.get(id)).filter((row) => row != null);
    },
    async getDefault(provider) {
      const existing = [...rows.values()].find((row) =>
        row.provider === provider && row.isDefault
      );
      if (existing) return existing;
      const now = new Date("2026-07-31T12:00:00Z");
      const row: IntegrationConnectionRow = {
        id: `default-${provider}`,
        alias: `${provider}-default`,
        provider,
        displayName: `${provider} default`,
        isDefault: true,
        config: {},
        enabled: true,
        testedAt: new Date(0),
        createdAt: now,
        updatedAt: now,
      };
      rows.set(row.id, row);
      return row;
    },
    async create(input) {
      sequence += 1;
      const now = new Date("2026-07-31T12:00:00Z");
      const row: IntegrationConnectionRow = {
        id: `connection-${sequence}`,
        ...input,
        isDefault: false,
        enabled: false,
        testedAt: null,
        createdAt: now,
        updatedAt: now,
      };
      rows.set(row.id, row);
      return row;
    },
    async update(id, input) {
      const current = rows.get(id);
      if (!current) return null;
      const row = {
        ...current,
        ...input,
        enabled: false,
        testedAt: null,
        updatedAt: new Date("2026-07-31T12:01:00Z"),
      };
      rows.set(id, row);
      return row;
    },
    async delete(id) {
      return rows.delete(id);
    },
    async markTested(id, testedAt) {
      const current = rows.get(id);
      if (!current) return null;
      const row = { ...current, testedAt, updatedAt: testedAt };
      rows.set(id, row);
      return row;
    },
    async setEnabled(id, enabled) {
      const current = rows.get(id);
      if (!current) return null;
      const row = { ...current, enabled };
      rows.set(id, row);
      return row;
    },
    async ensureDefault(provider) {
      return (await this.getDefault(provider))!;
    },
  };
}

const noProfiles = {
  async list() {
    return [];
  },
} as unknown as ProfileStore;

const CUSTOM = JSON.stringify({
  provider: "customco",
  protocol: "http",
  credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "customco-token", template: "Bearer {}" }] },
  hosts: ["customco.io"],
  operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/projects/*/issues/" } }],
});

async function spawn(deps: IntegrationDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  // Default a fake logo store so tests that don't exercise logos never hit the
  // real getDb() defaults (no DB in unit tests). Explicit deps override these.
  const withDefaults: IntegrationDeps = {
    connectorLogos: fakeLogoStore().store,
    connections: fakeConnectionStore(),
    profiles: noProfiles,
    googleExchange: async () => ({
      accessToken: "host-only-token",
      expiresAt: new Date("2026-07-31T12:07:00Z"),
    }),
    ...deps,
  };
  const srv = buildServer(app, (router) => registerIntegration(router, withDefaults));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(
      IntegrationService,
      createConnectTransport({ baseUrl: `${url}/rpc`, httpVersion: "1.1" }),
    ),
    close: () => new Promise<void>((res, rej) => srv.close((e) => (e ? rej(e) : res()))),
  };
}

async function expectErr(p: Promise<unknown>, code: Code) {
  try {
    await p;
    throw new Error(`expected ${Code[code]}`);
  } catch (e) {
    if (!(e instanceof ConnectError)) throw e;
    expect(e.code).toBe(code);
  }
}

describe("IntegrationService (native)", () => {
  test("anon → Unauthenticated; member → PermissionDenied", async () => {
    const anon = await spawn({ getSession: makeGetSession(null), connectors: fakeStore().store });
    try {
      await expectErr(anon.client.listConnectors({}), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store });
    try {
      await expectErr(mem.client.listConnectors({}), Code.PermissionDenied);
      await expectErr(mem.client.listConnections({}), Code.PermissionDenied);
      await expectErr(mem.client.upsertConnector({ configJson: CUSTOM }), Code.PermissionDenied);
      await expectErr(mem.client.deleteConnector({ provider: "customco" }), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("Google Cloud connection requires a successful WIF test before enable", async () => {
    const connections = fakeConnectionStore();
    const exchanged: string[] = [];
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore().store,
      connections,
      profiles: noProfiles,
      issuer: "https://tenant.example/api/v1/integrations/google-cloud/oidc",
      deploymentId: "tenant.example",
      now: () => new Date("2026-07-31T12:02:00Z"),
      googleExchange: async (_config, identity) => {
        exchanged.push(identity.connectionId);
        return { accessToken: "host-only-token", expiresAt: new Date("2026-07-31T12:07:00Z") };
      },
    });
    try {
      const created = await s.client.createConnection({
        alias: "prod-readonly",
        provider: "gcp",
        displayName: "Production read only",
        googleCloud: {
          workloadIdentityProvider:
            "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/prod",
          serviceAccountEmail: "engrams-reader@example-project.iam.gserviceaccount.com",
          endpoints: ["compute.googleapis.com", "tunnel.cloudproxy.app"],
        },
      });
      const id = created.connection!.id;
      expect(created.connection!.enabled).toBe(false);
      await expectErr(s.client.setConnectionEnabled({ id, enabled: true }), Code.FailedPrecondition);

      const setup = await s.client.getGoogleCloudSetup({ id });
      expect(setup.audience).toBe(
        "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/prod",
      );
      expect(setup.terraform).toContain(`assertion.engrams_connection == '${id}'`);
      // A-claims: the organization claim carries the DEPLOYMENT id, not the
      // issuer URL (the URL already rides in issuer_uri).
      expect(setup.terraform).toContain(
        "assertion.engrams_organization == 'tenant.example'",
      );
      expect(setup.terraform).toContain(
        'issuer_uri        = "https://tenant.example/api/v1/integrations/google-cloud/oidc"',
      );
      expect(setup.gcloudScript).toContain("roles/iam.workloadIdentityUser");
      expect(setup.terraform).not.toContain("private_key");

      // An operator pastes this into a shell. Without `set -euo pipefail` a
      // failed pool creation is invisible — the next command runs anyway and
      // the script "succeeds" with a half-built pool.
      expect(setup.gcloudScript.startsWith("#!/usr/bin/env bash\nset -euo pipefail\n")).toBe(true);
      // Re-running the setup is the normal thing to do after editing
      // endpoints, so creation is describe-then-create rather than an
      // ALREADY_EXISTS error.
      expect(setup.gcloudScript).toContain("workload-identity-pools describe engrams");
      expect(setup.gcloudScript).toContain("workload-identity-pools providers describe prod");

      // Terraform resource names are ADDRESSES. Two connections in one project
      // both emitted `...engrams`, so applying the second setup silently
      // redefined the first. The address is derived from the provider id.
      expect(setup.terraform).toContain('resource "google_iam_workload_identity_pool" "engrams_prod"');
      expect(setup.terraform).toContain(
        'workload_identity_pool_id          = google_iam_workload_identity_pool.engrams_prod.workload_identity_pool_id',
      );
      expect(setup.terraform).not.toContain('"google_iam_workload_identity_pool" "engrams"\n');

      expect((await s.client.testConnection({ id })).ok).toBe(true);
      expect(exchanged).toEqual([id]);
      expect((await s.client.setConnectionEnabled({ id, enabled: true })).connection!.enabled).toBe(true);

      const updated = await s.client.updateConnection({
        id,
        alias: "prod-readonly",
        displayName: "Production read only",
        googleCloud: created.connection!.googleCloud,
      });
      expect(updated.connection!.enabled).toBe(false);
      expect(updated.connection!.testedAt).toBe("");
      expect((await s.client.deleteConnection({ id })).deleted).toBe(true);
    } finally {
      await s.close();
    }
  });

  test("Google Cloud connections require a public HTTPS issuer", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore().store,
      issuer: "http://127.0.0.1:8787/api/v1/integrations/google-cloud/oidc",
    });
    try {
      await expectErr(s.client.createConnection({
        alias: "prod-readonly",
        provider: "gcp",
        displayName: "Production read only",
        googleCloud: {
          workloadIdentityProvider:
            "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/prod",
          serviceAccountEmail: "engrams-reader@example-project.iam.gserviceaccount.com",
          endpoints: ["compute.googleapis.com"],
        },
      }), Code.FailedPrecondition);
    } finally {
      await s.close();
    }
  });

  test("a provider default connection cannot be deleted", async () => {
    const connections = fakeConnectionStore();
    await connections.ensureDefault("github", "GitHub (default)");
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore().store,
      connections,
      profiles: noProfiles,
    });
    try {
      await expectErr(
        s.client.deleteConnection({ id: "default-github" }),
        Code.FailedPrecondition,
      );
    } finally {
      await s.close();
    }
  });

  test("ListConnectors merges built-in seeds (read-only) with custom rows", async () => {
    const custom: ConnectorRow = {
      provider: "customco",
      config: JSON.parse(CUSTOM),
      createdAt: new Date("2026-01-02T00:00:00Z"),
      updatedAt: new Date("2026-01-02T00:00:00Z"),
    };
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore([custom]).store,
    });
    try {
      const r = await s.client.listConnectors({});
      const byProvider = new Map(r.connectors.map((c) => [c.provider, c]));
      expect(byProvider.get("github")?.builtin).toBe(true); // file seed
      expect(byProvider.get("datadog")?.builtin).toBe(true); // file seed
      expect(byProvider.get("customco")?.builtin).toBe(false); // custom
    } finally {
      await s.close();
    }
  });

  test("UpsertConnector validates + stores a custom connector", async () => {
    const { store, rec } = fakeStore();
    const connections = fakeConnectionStore();
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: store,
      connections,
    });
    try {
      const r = await s.client.upsertConnector({ configJson: CUSTOM });
      expect(rec.upserts).toHaveLength(1);
      expect(rec.upserts[0]!.provider).toBe("customco");
      expect(r.connector?.provider).toBe("customco");
      expect(r.connector?.builtin).toBe(false);
      expect((await connections.getDefault("customco"))?.id).toBe("default-customco");
    } finally {
      await s.close();
    }
  });

  test("UpsertConnector rejects a built-in provider", async () => {
    const evil = JSON.stringify({ ...JSON.parse(CUSTOM), provider: "github" });
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store });
    try {
      await expectErr(s.client.upsertConnector({ configJson: evil }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });

  test("UpsertConnector rejects invalid JSON + invalid connectors", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store });
    try {
      await expectErr(s.client.upsertConnector({ configJson: "{not json" }), Code.InvalidArgument);
      // valid JSON, invalid connector (overbroad host wildcard rejected by parseConnector hardening)
      const bad = JSON.stringify({ ...JSON.parse(CUSTOM), hosts: ["*"] });
      await expectErr(s.client.upsertConnector({ configJson: bad }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });

  test("DeleteConnector deletes custom; rejects built-in", async () => {
    const custom: ConnectorRow = {
      provider: "customco",
      config: JSON.parse(CUSTOM),
      createdAt: new Date(),
      updatedAt: new Date(),
    };
    const { store, rec } = fakeStore([custom]);
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: store });
    try {
      const r = await s.client.deleteConnector({ provider: "customco" });
      expect(r.deleted).toBe(true);
      expect(rec.deletes).toEqual(["customco"]);
      await expectErr(s.client.deleteConnector({ provider: "github" }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });
});

describe("IntegrationService — connector status (redesign)", () => {
  const adminDeps = (names: string[]): IntegrationDeps => ({
    getSession: makeGetSession("a", "admin"),
    connectors: fakeStore().store,
    orgSecret: fakeOrgSecret(names).client,
    mint: fakeMint(),
  });

  test("inject connected ⇔ secretRef present; mint connected ⇔ all required fields present", async () => {
    const s = await spawn(adminDeps(["datadog-api-key", "datadog-app-key", "github_app.app_id", "github_app.private_key_pem"]));
    try {
      const by = new Map((await s.client.listConnectors({})).connectors.map((c) => [c.provider, c]));
      expect(by.get("datadog")?.status).toBe("connected"); // both inject secretRefs present
      expect(by.get("github")?.status).toBe("connected"); // both mint fields present
    } finally {
      await s.close();
    }
  });

  test("available when credentials are absent", async () => {
    const s = await spawn(adminDeps([]));
    try {
      const by = new Map((await s.client.listConnectors({})).connectors.map((c) => [c.provider, c]));
      expect(by.get("datadog")?.status).toBe("available");
      expect(by.get("github")?.status).toBe("available");
    } finally {
      await s.close();
    }
  });

  test("mint stays available until EVERY required field is present", async () => {
    const s = await spawn(adminDeps(["github_app.app_id"])); // missing private_key_pem
    try {
      const by = new Map((await s.client.listConnectors({})).connectors.map((c) => [c.provider, c]));
      expect(by.get("github")?.status).toBe("available");
    } finally {
      await s.close();
    }
  });
});

describe("GetIntegrationCatalog (member-readable)", () => {
  test("anon → Unauthenticated; a member gets the derived catalog", async () => {
    invalidateRegistry();
    const anon = await spawn({ getSession: makeGetSession(null), connectors: fakeStore().store });
    try {
      await expectErr(anon.client.getIntegrationCatalog({}), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store });
    try {
      const r = await mem.client.getIntegrationCatalog({});
      const by = new Map(r.providers.map((p) => [p.provider, p]));
      expect(by.get("github")?.display?.name).toBe("GitHub");
      expect(by.get("github")?.defaultConnectionId).toBe("default-github");
      expect(by.get("github")?.display?.icon?.mono).toBe("GH");
      expect(by.get("datadog")?.credentialSource).toBe("inject");
      const ghCaps = new Map(by.get("github")!.capabilities.map((c) => [c.action, c]));
      expect(ghCaps.get("pulls:write")?.access).toBe("write");
      expect(ghCaps.get("pulls:write")?.asset).toBe("pull_request");
      expect(ghCaps.get("contents:read")?.access).toBe("read");
    } finally {
      await mem.close();
    }
  });

  test("carries no secret material", async () => {
    invalidateRegistry();
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store });
    try {
      const r = await mem.client.getIntegrationCatalog({});
      // Match the inject KEYS as quoted JSON ("secretRef"/"template"), not bare
      // substrings — else a legitimate capability action like SendGrid's
      // `templates:read` trips the `template` guard. Secret VALUES/kinds stay bare.
      expect(JSON.stringify(r)).not.toMatch(/"secretRef"|"template"|datadog-api-key|github_app|DD-API-KEY/);
    } finally {
      await mem.close();
    }
  });
});

describe("SetMintCredential", () => {
  test("anon → Unauthenticated; member → PermissionDenied", async () => {
    const anon = await spawn({ getSession: makeGetSession(null), connectors: fakeStore().store, mint: fakeMint() });
    try {
      await expectErr(anon.client.setMintCredential({ provider: "github", kind: "github_app", values: {} }), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store, mint: fakeMint() });
    try {
      await expectErr(mem.client.setMintCredential({ provider: "github", kind: "github_app", values: {} }), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("seals each non-blank field as `<kind>.<field>`", async () => {
    const os = fakeOrgSecret();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, orgSecret: os.client, mint: fakeMint() });
    try {
      const r = await s.client.setMintCredential({
        provider: "github",
        kind: "github_app",
        values: { app_id: "1357924", private_key_pem: "-----BEGIN RSA PRIVATE KEY-----" },
      });
      expect(new Set(r.secretNames)).toEqual(new Set(["github_app.app_id", "github_app.private_key_pem"]));
      expect(os.puts.map((p) => p.name).sort()).toEqual(["github_app.app_id", "github_app.private_key_pem"]);
    } finally {
      await s.close();
    }
  });

  test("skips blank values (leave-unchanged for the Replace flow)", async () => {
    const os = fakeOrgSecret();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, orgSecret: os.client, mint: fakeMint() });
    try {
      const r = await s.client.setMintCredential({ provider: "github", kind: "github_app", values: { app_id: "1357924", private_key_pem: "" } });
      expect(r.secretNames).toEqual(["github_app.app_id"]);
      expect(os.puts).toHaveLength(1);
    } finally {
      await s.close();
    }
  });

  test("rejects unknown kind / unknown field / provider mismatch", async () => {
    const os = fakeOrgSecret();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, orgSecret: os.client, mint: fakeMint() });
    try {
      await expectErr(s.client.setMintCredential({ provider: "github", kind: "nope", values: {} }), Code.InvalidArgument);
      await expectErr(s.client.setMintCredential({ provider: "github", kind: "github_app", values: { bogus: "x" } }), Code.InvalidArgument);
      await expectErr(s.client.setMintCredential({ provider: "gitlab", kind: "github_app", values: { app_id: "1" } }), Code.InvalidArgument);
      expect(os.puts).toHaveLength(0);
    } finally {
      await s.close();
    }
  });
});

describe("UploadConnectorLogo + catalog overlay", () => {
  test("anon → Unauthenticated; member → PermissionDenied", async () => {
    const anon = await spawn({ getSession: makeGetSession(null), connectors: fakeStore().store });
    try {
      await expectErr(anon.client.uploadConnectorLogo({ provider: "github", data: PNG_BYTES, mediaType: "" }), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store });
    try {
      await expectErr(mem.client.uploadConnectorLogo({ provider: "github", data: PNG_BYTES, mediaType: "" }), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("stores a sniffed PNG / SVG for an existing connector + returns the serve URL", async () => {
    invalidateRegistry();
    const fl = fakeLogoStore();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, connectorLogos: fl.store });
    try {
      const r = await s.client.uploadConnectorLogo({ provider: "github", data: PNG_BYTES, mediaType: "" });
      expect(r.logoUrl).toBe("/api/v1/integrations/github/logo");
      expect(fl.rows.get("github")?.mediaType).toBe("image/png");
      await s.client.uploadConnectorLogo({ provider: "datadog", data: SVG_BYTES, mediaType: "" });
      expect(fl.rows.get("datadog")?.mediaType).toBe("image/svg+xml");
    } finally {
      await s.close();
    }
  });

  test("rejects unknown provider / non-image / oversized; empty data clears", async () => {
    invalidateRegistry();
    const fl = fakeLogoStore({ github: { mediaType: "image/png", data: Buffer.from(PNG_BYTES) } });
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, connectorLogos: fl.store });
    try {
      await expectErr(s.client.uploadConnectorLogo({ provider: "nope", data: PNG_BYTES, mediaType: "" }), Code.InvalidArgument);
      await expectErr(s.client.uploadConnectorLogo({ provider: "github", data: new TextEncoder().encode("not an image"), mediaType: "" }), Code.InvalidArgument);
      await expectErr(s.client.uploadConnectorLogo({ provider: "github", data: new Uint8Array(512 * 1024 + 1), mediaType: "" }), Code.InvalidArgument);
      const cleared = await s.client.uploadConnectorLogo({ provider: "github", data: new Uint8Array(0), mediaType: "" });
      expect(cleared.logoUrl).toBe("");
      expect(fl.rows.has("github")).toBe(false);
    } finally {
      await s.close();
    }
  });

  test("getIntegrationCatalog overlays icon.logo only for providers that have one", async () => {
    invalidateRegistry();
    const fl = fakeLogoStore({ github: { mediaType: "image/png", data: Buffer.from(PNG_BYTES) } });
    const s = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store, connectorLogos: fl.store });
    try {
      const by = new Map((await s.client.getIntegrationCatalog({})).providers.map((p) => [p.provider, p]));
      expect(by.get("github")?.display?.icon?.logo).toBe("/api/v1/integrations/github/logo");
      expect(by.get("datadog")?.display?.icon?.logo).toBe(""); // no uploaded logo → monogram
    } finally {
      await s.close();
    }
  });

  test("deleteConnector also clears the connector's logo", async () => {
    invalidateRegistry();
    const custom: ConnectorRow = { provider: "customco", config: JSON.parse(CUSTOM), createdAt: new Date(), updatedAt: new Date() };
    const fl = fakeLogoStore({ customco: { mediaType: "image/png", data: Buffer.from(PNG_BYTES) } });
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore([custom]).store, connectorLogos: fl.store });
    try {
      await s.client.deleteConnector({ provider: "customco" });
      expect(fl.rows.has("customco")).toBe(false);
    } finally {
      await s.close();
    }
  });
});

describe("TestConnector", () => {
  test("anon → Unauthenticated; member → PermissionDenied", async () => {
    const anon = await spawn({ getSession: makeGetSession(null), connectors: fakeStore().store, mint: fakeMint() });
    try {
      await expectErr(anon.client.testConnector({ provider: "datadog", draftValues: {} }), Code.Unauthenticated);
    } finally {
      await anon.close();
    }
    const mem = await spawn({ getSession: makeGetSession("m"), connectors: fakeStore().store, mint: fakeMint() });
    try {
      await expectErr(mem.client.testConnector({ provider: "datadog", draftValues: {} }), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("builds the inject spec with ALL headers, drafts keyed by secret ref (ADR 0058)", async () => {
    let captured: Record<string, unknown> | undefined;
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore().store,
      mint: fakeMint((spec) => {
        captured = spec as Record<string, unknown>;
        return { ok: true, message: "Reached api.datadoghq.com" };
      }),
    });
    try {
      const r = await s.client.testConnector({
        provider: "datadog",
        draftValues: { "datadog-api-key": "dd-key", "datadog-app-key": "dd-app" },
      });
      expect(r.ok).toBe(true);
      expect(r.message).toContain("datadoghq");
      // ADR 0058: EVERY injected header is probed; drafts keyed by org-secret ref.
      // The probe targets the connector's `test.path` (Datadog's `/` 307s to a
      // public page, so the test must hit an endpoint that needs both keys).
      expect(captured).toMatchObject({
        provider: "datadog",
        host: "api.datadoghq.com",
        source: "inject",
        testPath: "/api/v1/dashboard",
        injects: [
          { header: "DD-API-KEY", template: "{}", secretRef: "datadog-api-key", draftSecret: "dd-key" },
          {
            header: "DD-APPLICATION-KEY",
            template: "{}",
            secretRef: "datadog-app-key",
            draftSecret: "dd-app",
          },
        ],
      });
    } finally {
      await s.close();
    }
  });

  test("builds the mint spec (kind + draft fields) from the registry", async () => {
    let captured: Record<string, unknown> | undefined;
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      connectors: fakeStore().store,
      mint: fakeMint((spec) => {
        captured = spec as Record<string, unknown>;
        return { ok: true, message: "Minted" };
      }),
    });
    try {
      await s.client.testConnector({ provider: "github", draftValues: { app_id: "1357924" } });
      expect(captured).toMatchObject({ provider: "github", source: "mint", kind: "github_app" });
      expect((captured!.draftFields as Record<string, string>).app_id).toBe("1357924");
    } finally {
      await s.close();
    }
  });

  test("rejects an unknown connector", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: fakeStore().store, mint: fakeMint() });
    try {
      await expectErr(s.client.testConnector({ provider: "nope", draftValues: {} }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });
});
