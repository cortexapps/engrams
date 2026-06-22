/**
 * Native OrgSecretService proxy tests (ADR 0057 A1) — injected deps, no network.
 *
 *   - all RPCs are admin-only (401 anon, 403 member)
 *   - admin can put/list/delete; the value is forwarded verbatim to the
 *     coordinator (which seals it) and never echoed in the response
 */

import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerOrgSecret } from "../rpc/org-secret.ts";
import type { OrgSecretDeps, GetSession, OrgSecretClient, OrgSecretMetaRow } from "../rpc/org-secret.ts";
import { OrgSecretService } from "../gen/engram/app/v1/org_secret_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

interface Recorded {
  put: Array<{ name: string; value: string }>;
  deleted: string[];
}

function fakeStore(seed: OrgSecretMetaRow[] = []): { client: OrgSecretClient; rec: Recorded } {
  const rec: Recorded = { put: [], deleted: [] };
  const client: OrgSecretClient = {
    async listSecrets() {
      return { secrets: seed };
    },
    async putSecret(req) {
      rec.put.push(req);
      return {
        secret: {
          name: req.name,
          keyId: "env:KEK:v1",
          createdAt: "2026-01-01T00:00:00Z",
          updatedAt: "2026-01-01T00:00:00Z",
        },
      };
    },
    async deleteSecret(req) {
      rec.deleted.push(req.name);
      return { deleted: true };
    },
  };
  return { client, rec };
}

async function spawn(deps: OrgSecretDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerOrgSecret(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(
      OrgSecretService,
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

const seeded: OrgSecretMetaRow = {
  name: "datadog-api-key",
  keyId: "env:KEK:v1",
  createdAt: "2026-01-02T00:00:00Z",
  updatedAt: "2026-01-02T00:00:00Z",
};

describe("OrgSecretService (native)", () => {
  test("anon ListSecrets → Unauthenticated", async () => {
    const s = await spawn({ getSession: makeGetSession(null), orgSecret: fakeStore().client });
    try {
      await expectErr(s.client.listSecrets({}), Code.Unauthenticated);
    } finally {
      await s.close();
    }
  });

  test("member ListSecrets/PutSecret/DeleteSecret → PermissionDenied", async () => {
    const s = await spawn({ getSession: makeGetSession("m"), orgSecret: fakeStore().client });
    try {
      await expectErr(s.client.listSecrets({}), Code.PermissionDenied);
      await expectErr(
        s.client.putSecret({ name: "k", value: "v" }),
        Code.PermissionDenied,
      );
      await expectErr(s.client.deleteSecret({ name: "k" }), Code.PermissionDenied);
    } finally {
      await s.close();
    }
  });

  test("admin lists metadata only (no value field on the wire)", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"),
      orgSecret: fakeStore([seeded]).client,
    });
    try {
      const r = await s.client.listSecrets({});
      expect(r.secrets.map((x) => x.name)).toContain("datadog-api-key");
      // OrgSecretMeta carries no value field at all.
      expect(Object.keys(r.secrets[0])).not.toContain("value");
    } finally {
      await s.close();
    }
  });

  test("admin PutSecret forwards the value to the coordinator; response carries no value", async () => {
    const { client, rec } = fakeStore();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), orgSecret: client });
    try {
      const r = await s.client.putSecret({ name: "sentry-token", value: "sk-live-xyz" });
      expect(rec.put).toHaveLength(1);
      expect(rec.put[0]).toEqual({ name: "sentry-token", value: "sk-live-xyz" });
      expect(r.secret?.name).toBe("sentry-token");
      expect(Object.keys(r.secret ?? {})).not.toContain("value");
    } finally {
      await s.close();
    }
  });

  test("admin DeleteSecret delegates", async () => {
    const { client, rec } = fakeStore();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), orgSecret: client });
    try {
      const r = await s.client.deleteSecret({ name: "datadog-api-key" });
      expect(r.deleted).toBe(true);
      expect(rec.deleted).toEqual(["datadog-api-key"]);
    } finally {
      await s.close();
    }
  });
});
