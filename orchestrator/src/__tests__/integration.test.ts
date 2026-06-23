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
import type { IntegrationDeps, GetSession } from "../rpc/integration.ts";
import type { ConnectorStore, ConnectorRow } from "../db/connectors.ts";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

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

const SENTRY = JSON.stringify({
  provider: "sentry",
  protocol: "http",
  credential: { source: "inject", inject: { header: "Authorization", secretRef: "sentry-token", template: "Bearer {}" } },
  hosts: ["sentry.io"],
  operations: [{ grants: ["issues:read"], match: { method: "GET", path: "/api/0/projects/*/issues/" } }],
});

async function spawn(deps: IntegrationDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerIntegration(router, deps));
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
      await expectErr(mem.client.upsertConnector({ configJson: SENTRY }), Code.PermissionDenied);
      await expectErr(mem.client.deleteConnector({ provider: "sentry" }), Code.PermissionDenied);
    } finally {
      await mem.close();
    }
  });

  test("ListConnectors merges built-in seeds (read-only) with custom rows", async () => {
    const custom: ConnectorRow = {
      provider: "sentry",
      config: JSON.parse(SENTRY),
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
      expect(byProvider.get("sentry")?.builtin).toBe(false); // custom
    } finally {
      await s.close();
    }
  });

  test("UpsertConnector validates + stores a custom connector", async () => {
    const { store, rec } = fakeStore();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: store });
    try {
      const r = await s.client.upsertConnector({ configJson: SENTRY });
      expect(rec.upserts).toHaveLength(1);
      expect(rec.upserts[0]!.provider).toBe("sentry");
      expect(r.connector?.provider).toBe("sentry");
      expect(r.connector?.builtin).toBe(false);
    } finally {
      await s.close();
    }
  });

  test("UpsertConnector rejects a built-in provider", async () => {
    const evil = JSON.stringify({ ...JSON.parse(SENTRY), provider: "github" });
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
      const bad = JSON.stringify({ ...JSON.parse(SENTRY), hosts: ["*"] });
      await expectErr(s.client.upsertConnector({ configJson: bad }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });

  test("DeleteConnector deletes custom; rejects built-in", async () => {
    const custom: ConnectorRow = {
      provider: "sentry",
      config: JSON.parse(SENTRY),
      createdAt: new Date(),
      updatedAt: new Date(),
    };
    const { store, rec } = fakeStore([custom]);
    const s = await spawn({ getSession: makeGetSession("a", "admin"), connectors: store });
    try {
      const r = await s.client.deleteConnector({ provider: "sentry" });
      expect(r.deleted).toBe(true);
      expect(rec.deletes).toEqual(["sentry"]);
      await expectErr(s.client.deleteConnector({ provider: "github" }), Code.InvalidArgument);
    } finally {
      await s.close();
    }
  });
});
