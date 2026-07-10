/**
 * ApiKeyService tests (ADR 0086).
 *
 * Group 1 — RBAC + handler logic with a stubbed backend (no DB, no network):
 *   the org-secret.test.ts harness. Proves the admin gate, input validation,
 *   the no-orphan-on-mint-failure cleanup, and the revoke guards.
 *
 * Group 2 — live better-auth + drizzle (DB-gated, same skipIf pattern as
 * db.test.ts; runs in CI where the orchestrator lane has Postgres):
 *   - management RPCs against the REAL backend (service user + plugin mint)
 *   - the mock-session seam: a minted key resolves through the REAL
 *     getSessionFromHeaders to {id, role} — x-api-key AND Bearer engk_ forms
 *   - true e2e: a server with REAL session resolution authorizes an
 *     admin-role key, rejects a user-role key (PermissionDenied — RBAC), and
 *     rejects junk/expired keys as Unauthenticated (NOT Internal — pins the
 *     catch-to-null wrapper)
 *   - expiry: expired keys are auto-deleted at verify; the orphaned service
 *     user is swept on the next List
 *   - lockdown: the plugin's own HTTP endpoints 404 even with a valid cookie
 */

import { expect, test, describe } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";
import { eq } from "drizzle-orm";

import { buildServer } from "../server.ts";
import { registerApiKeys, isServiceAccountEmail } from "../rpc/api-key.ts";
import type { ApiKeyBackend, ApiKeyDeps, GetSession, MintedKey } from "../rpc/api-key.ts";
import { ApiKeyService } from "../gen/engram/app/v1/api_key_pb.ts";
import { checkDb } from "../db/client.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role } } : null);
}

async function spawn(deps?: ApiKeyDeps, mountAuth = false) {
  const app = new Hono();
  if (mountAuth) {
    const { default: authRoute } = await import("../routes/auth.ts");
    app.route("/", authRoute);
  }
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerApiKeys(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    url,
    client: createClient(
      ApiKeyService,
      createConnectTransport({ baseUrl: `${url}/rpc`, httpVersion: "1.1" }),
    ),
    /** A client that sends extra headers on every RPC (API-key auth). */
    keyedClient: (headers: Record<string, string>) =>
      createClient(
        ApiKeyService,
        createConnectTransport({
          baseUrl: `${url}/rpc`,
          httpVersion: "1.1",
          interceptors: [
            (next) => (req) => {
              for (const [k, v] of Object.entries(headers)) req.header.set(k, v);
              return next(req);
            },
          ],
        }),
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

// ---------------------------------------------------------------------------
// Group 1 — stubbed backend
// ---------------------------------------------------------------------------

interface Recorded {
  createdUsers: Array<{ name: string; role: string }>;
  deletedUsers: string[];
  minted: Array<{ userId: string; name: string; expiresIn?: number }>;
  swept: number;
}

function fakeBackend(opts?: { mintFails?: boolean }): { backend: ApiKeyBackend; rec: Recorded } {
  const rec: Recorded = { createdUsers: [], deletedUsers: [], minted: [], swept: 0 };
  let nextUser = 0;
  const keys = new Map<string, { referenceId: string; email: string }>();
  const backend: ApiKeyBackend = {
    async createServiceUser(input) {
      rec.createdUsers.push(input);
      return { id: `svc-${nextUser++}` };
    },
    async deleteUser(userId) {
      rec.deletedUsers.push(userId);
      for (const [id, k] of keys) if (k.referenceId === userId) keys.delete(id);
    },
    async mintKey(input): Promise<MintedKey> {
      if (opts?.mintFails) throw new Error("plugin exploded");
      rec.minted.push(input);
      const id = `key-${rec.minted.length}`;
      keys.set(id, {
        referenceId: input.userId,
        email: `apikey+${input.userId}@service.local`,
      });
      return {
        id,
        key: "engk_plaintext-once",
        start: "engk_plaint",
        prefix: "engk_",
        createdAt: new Date("2026-07-09T00:00:00Z"),
        expiresAt: input.expiresIn ? new Date(Date.now() + input.expiresIn * 1000) : null,
      };
    },
    async listKeys() {
      return [...keys.entries()].map(([id, k], i) => ({
        id,
        name: `key ${i}`,
        role: "user",
        start: "engk_plaint",
        prefix: "engk_",
        createdAt: new Date("2026-07-09T00:00:00Z"),
        expiresAt: null,
        lastRequest: null,
      }));
    },
    async findKey(id) {
      return keys.get(id) ?? null;
    },
    async sweepOrphanServiceUsers() {
      rec.swept++;
    },
  };
  return { backend, rec };
}

describe("ApiKeyService (native, stubbed backend)", () => {
  test("anon → Unauthenticated on all RPCs", async () => {
    const s = await spawn({ getSession: makeGetSession(null), backend: fakeBackend().backend });
    try {
      await expectErr(s.client.createApiKey({ name: "k", role: "user", expiresAt: "" }), Code.Unauthenticated);
      await expectErr(s.client.listApiKeys({}), Code.Unauthenticated);
      await expectErr(s.client.revokeApiKey({ id: "x" }), Code.Unauthenticated);
    } finally {
      await s.close();
    }
  });

  test("member → PermissionDenied on all RPCs", async () => {
    const s = await spawn({ getSession: makeGetSession("m"), backend: fakeBackend().backend });
    try {
      await expectErr(s.client.createApiKey({ name: "k", role: "user", expiresAt: "" }), Code.PermissionDenied);
      await expectErr(s.client.listApiKeys({}), Code.PermissionDenied);
      await expectErr(s.client.revokeApiKey({ id: "x" }), Code.PermissionDenied);
    } finally {
      await s.close();
    }
  });

  test("create validates name / role / expires_at", async () => {
    const { backend, rec } = fakeBackend();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      await expectErr(s.client.createApiKey({ name: "  ", role: "user", expiresAt: "" }), Code.InvalidArgument);
      await expectErr(s.client.createApiKey({ name: "k", role: "root", expiresAt: "" }), Code.InvalidArgument);
      await expectErr(
        s.client.createApiKey({ name: "k", role: "user", expiresAt: "not-a-date" }),
        Code.InvalidArgument,
      );
      // Under the 1-day plugin floor (1 hour out) → rejected.
      await expectErr(
        s.client.createApiKey({
          name: "k",
          role: "user",
          expiresAt: new Date(Date.now() + 3600_000).toISOString(),
        }),
        Code.InvalidArgument,
      );
      // Nothing reached the backend.
      expect(rec.createdUsers).toHaveLength(0);
      expect(rec.minted).toHaveLength(0);
    } finally {
      await s.close();
    }
  });

  test("admin create → service user with the key's role, plaintext returned once", async () => {
    const { backend, rec } = fakeBackend();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      const twoDays = new Date(Date.now() + 2 * 24 * 3600_000).toISOString();
      const r = await s.client.createApiKey({ name: "ci-bot", role: "admin", expiresAt: twoDays });
      expect(rec.createdUsers).toEqual([{ name: "ci-bot", role: "admin" }]);
      expect(rec.minted).toHaveLength(1);
      expect(rec.minted[0]!.userId).toBe("svc-0");
      // expiresIn ≈ 2 days in seconds.
      expect(rec.minted[0]!.expiresIn).toBeGreaterThan(2 * 24 * 3600 - 60);
      expect(rec.minted[0]!.expiresIn).toBeLessThanOrEqual(2 * 24 * 3600 + 60);
      expect(r.key).toBe("engk_plaintext-once");
      expect(r.meta?.role).toBe("admin");
      expect(r.meta?.start).toBe("engk_plaint");
      expect(r.meta?.expiresAt).not.toBe("");
    } finally {
      await s.close();
    }
  });

  test("no expiry → no expiresIn forwarded, meta.expiresAt empty", async () => {
    const { backend, rec } = fakeBackend();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      const r = await s.client.createApiKey({ name: "forever", role: "user", expiresAt: "" });
      expect(rec.minted[0]!.expiresIn).toBeUndefined();
      expect(r.meta?.expiresAt).toBe("");
    } finally {
      await s.close();
    }
  });

  test("mint failure deletes the just-created service user (no orphan)", async () => {
    const { backend, rec } = fakeBackend({ mintFails: true });
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      await expect(s.client.createApiKey({ name: "k", role: "user", expiresAt: "" })).rejects.toThrow();
      expect(rec.createdUsers).toHaveLength(1);
      expect(rec.deletedUsers).toEqual(["svc-0"]);
    } finally {
      await s.close();
    }
  });

  test("list sweeps orphans and returns metadata only", async () => {
    const { backend, rec } = fakeBackend();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      await s.client.createApiKey({ name: "k", role: "user", expiresAt: "" });
      const r = await s.client.listApiKeys({});
      expect(rec.swept).toBe(1);
      expect(r.keys).toHaveLength(1);
      // Meta never carries key material — only the masked preview.
      expect(Object.keys(r.keys[0]!)).not.toContain("key");
      expect(r.keys[0]!.start).toBe("engk_plaint");
    } finally {
      await s.close();
    }
  });

  test("revoke: absent id is idempotent; service-account key deletes its user", async () => {
    const { backend, rec } = fakeBackend();
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend });
    try {
      expect((await s.client.revokeApiKey({ id: "nope" })).revoked).toBe(false);
      await s.client.createApiKey({ name: "k", role: "user", expiresAt: "" });
      expect((await s.client.revokeApiKey({ id: "key-1" })).revoked).toBe(true);
      expect(rec.deletedUsers).toEqual(["svc-0"]);
      // Second revoke: the key row is gone → idempotent false.
      expect((await s.client.revokeApiKey({ id: "key-1" })).revoked).toBe(false);
    } finally {
      await s.close();
    }
  });

  test("revoke refuses a key owned by a human user", async () => {
    const { backend } = fakeBackend();
    const human: ApiKeyBackend = {
      ...backend,
      async findKey() {
        return { referenceId: "u-human", email: "alice@example.com" };
      },
    };
    const s = await spawn({ getSession: makeGetSession("a", "admin"), backend: human });
    try {
      await expectErr(s.client.revokeApiKey({ id: "k" }), Code.FailedPrecondition);
    } finally {
      await s.close();
    }
  });

  test("isServiceAccountEmail matches only the convention", () => {
    expect(isServiceAccountEmail("apikey+abc@service.local")).toBe(true);
    expect(isServiceAccountEmail("alice@example.com")).toBe(false);
    expect(isServiceAccountEmail("apikey+abc@example.com")).toBe(false);
    expect(isServiceAccountEmail("bob+apikey@service.local")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Group 2 — live better-auth + drizzle (DB-gated)
// ---------------------------------------------------------------------------

describe("ApiKeyService (live DB + real plugin)", () => {
  test.skipIf(!dbReachable)(
    "mint → keyed requests authenticate with the key's role; revoke kills; expiry cleans up",
    async () => {
      const { getDb } = await import("../db/client.ts");
      const { apikey, user } = await import("../db/schema.ts");
      const { getSessionFromHeaders } = await import("../auth/session.ts");
      const db = getDb();

      // Management server: REAL backend, injected admin session (cookie
      // minting is the IAP bridge's job, tested elsewhere).
      const admin = await spawn({ getSession: makeGetSession("test-admin", "admin") });
      // E2E server: fully real deps — session resolution included.
      const e2e = await spawn();

      const cleanupUserIds: string[] = [];
      try {
        // --- mint an admin-role and a user-role key ---
        const adminKey = await admin.client.createApiKey({
          name: "e2e admin key",
          role: "admin",
          expiresAt: "",
        });
        const userKey = await admin.client.createApiKey({
          name: "e2e user key",
          role: "user",
          expiresAt: new Date(Date.now() + 2 * 24 * 3600_000).toISOString(),
        });
        expect(adminKey.key.startsWith("engk_")).toBe(true);
        expect(adminKey.meta!.start.startsWith("engk_")).toBe(true);
        // The stored row holds a hash, never the plaintext.
        const stored = await db
          .select({ key: apikey.key, referenceId: apikey.referenceId })
          .from(apikey)
          .where(eq(apikey.id, adminKey.meta!.id));
        expect(stored[0]!.key).not.toBe(adminKey.key);
        cleanupUserIds.push(stored[0]!.referenceId);
        const storedUser = await db
          .select({ referenceId: apikey.referenceId })
          .from(apikey)
          .where(eq(apikey.id, userKey.meta!.id));
        cleanupUserIds.push(storedUser[0]!.referenceId);

        // --- the seam: getSessionFromHeaders resolves the mock session ---
        const viaHeader = await getSessionFromHeaders(new Headers({ "x-api-key": adminKey.key }));
        expect(viaHeader?.user.role).toBe("admin");
        expect(isServiceAccountEmail(viaHeader?.user.email ?? "")).toBe(true);
        const viaBearer = await getSessionFromHeaders(
          new Headers({ authorization: `Bearer ${userKey.key}` }),
        );
        expect(viaBearer?.user.role).toBe("user");
        // Junk key → anonymous, not an exception (pins the wrapper).
        expect(await getSessionFromHeaders(new Headers({ "x-api-key": "engk_junk" }))).toBeNull();

        // --- true e2e: keyed RPCs against fully-real session resolution ---
        const viaAdminKey = await e2e.keyedClient({ "x-api-key": adminKey.key }).listApiKeys({});
        expect(viaAdminKey.keys.map((k) => k.id)).toContain(userKey.meta!.id);
        // user-role key on an admin surface → RBAC says no.
        await expectErr(
          e2e.keyedClient({ "x-api-key": userKey.key }).listApiKeys({}),
          Code.PermissionDenied,
        );
        // Bearer form works end-to-end too.
        const viaBearerRpc = await e2e
          .keyedClient({ authorization: `Bearer ${adminKey.key}` })
          .listApiKeys({});
        expect(viaBearerRpc.keys.length).toBeGreaterThanOrEqual(2);
        // Junk key → Unauthenticated, NOT Internal/Unknown.
        await expectErr(e2e.keyedClient({ "x-api-key": "engk_junk" }).listApiKeys({}), Code.Unauthenticated);

        // --- expiry: backdate, then the key both fails AND self-deletes ---
        await db
          .update(apikey)
          .set({ expiresAt: new Date(Date.now() - 1000) })
          .where(eq(apikey.id, userKey.meta!.id));
        expect(await getSessionFromHeaders(new Headers({ "x-api-key": userKey.key }))).toBeNull();
        const expiredRow = await db.select({ id: apikey.id }).from(apikey).where(eq(apikey.id, userKey.meta!.id));
        expect(expiredRow).toHaveLength(0);
        // The orphaned service user is swept by the next admin List.
        await admin.client.listApiKeys({});
        const orphan = await db.select({ id: user.id }).from(user).where(eq(user.id, storedUser[0]!.referenceId));
        expect(orphan).toHaveLength(0);

        // --- revoke: key stops working immediately; idempotent second call ---
        expect((await admin.client.revokeApiKey({ id: adminKey.meta!.id })).revoked).toBe(true);
        expect(await getSessionFromHeaders(new Headers({ "x-api-key": adminKey.key }))).toBeNull();
        const goneUser = await db.select({ id: user.id }).from(user).where(eq(user.id, stored[0]!.referenceId));
        expect(goneUser).toHaveLength(0);
        expect((await admin.client.revokeApiKey({ id: adminKey.meta!.id })).revoked).toBe(false);
      } finally {
        // Belt-and-braces cleanup for failed runs (cascade removes key rows).
        for (const id of cleanupUserIds) {
          const { user: u } = await import("../db/schema.ts");
          await db.delete(u).where(eq(u.id, id)).catch(() => {});
        }
        await admin.close();
        await e2e.close();
      }
    },
  );

  test.skipIf(!dbReachable)(
    "plugin HTTP endpoints are locked down (404 even with a valid session cookie)",
    async () => {
      const s = await spawn(undefined, /* mountAuth */ true);
      try {
        // Anonymous HTTP call: our hooks.before 404s BEFORE the plugin's own
        // 401 — the endpoint reads as nonexistent, not as auth-gated.
        const anon = await fetch(`${s.url}/api/auth/api-key/create`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ name: "sneaky" }),
        });
        expect(anon.status).toBe(404);

        // Cookie-authed call (fresh sign-up → session cookie): still 404.
        const email = `lockdown-${Date.now()}@example.com`;
        const signup = await fetch(`${s.url}/api/auth/sign-up/email`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ email, password: "hunter2hunter2", name: "Lockdown" }),
        });
        expect(signup.ok).toBe(true);
        const cookie = signup.headers.getSetCookie().map((c) => c.split(";")[0]).join("; ");
        const authed = await fetch(`${s.url}/api/auth/api-key/create`, {
          method: "POST",
          headers: { "content-type": "application/json", cookie },
          body: JSON.stringify({ name: "sneaky" }),
        });
        expect(authed.status).toBe(404);
        // The plugin's list endpoint is equally dead.
        const list = await fetch(`${s.url}/api/auth/api-key/list`, { headers: { cookie } });
        expect(list.status).toBe(404);
      } finally {
        await s.close();
      }
    },
  );
});
