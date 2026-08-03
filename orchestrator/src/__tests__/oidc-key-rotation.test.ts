/**
 * OIDC signing-key rotation (ADR 0109 closeout):
 *   - the pure scheduled step (`rotateOidcKeyIfDue`),
 *   - the explicit admin trigger route,
 *   - the live-PG store behavior: JWKS publishes BOTH kids inside the overlap
 *     window, drops the retiring kid after it, and concurrent rotation cannot
 *     produce two active keys.
 */

import { describe, expect, test } from "bun:test";
import { eq, inArray } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  makeIntegrationOidcKeyStore,
  type IntegrationOidcKeyStore,
  type OidcSigningKey,
} from "../db/integration-oidc-keys.ts";
import { integrationOidcKey as keyTable } from "../db/schema.ts";
import {
  rotateOidcKeyIfDue,
  OIDC_KEY_PUBLISH_OVERLAP_MS,
} from "../integrations/oidc-key-rotation.ts";
import { makeOidcKeyAdminRoute } from "../routes/oidc-key-admin.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const DAY_MS = 24 * 60 * 60 * 1000;

function fakeKey(kid: string, state: "active" | "retiring", createdAt: Date): OidcSigningKey {
  return {
    kid,
    publicJwk: { kty: "RSA" },
    privateKeyPem: "pem",
    state,
    createdAt,
    publishUntil: null,
  };
}

function fakeStore(published: OidcSigningKey[]): IntegrationOidcKeyStore & {
  rotations: number;
  creates: number;
} {
  const store = {
    rotations: 0,
    creates: 0,
    async getOrCreateActive(now: Date) {
      store.creates += 1;
      return fakeKey("created-kid", "active", now);
    },
    async listPublished() {
      return published;
    },
    async rotate(now: Date) {
      store.rotations += 1;
      return fakeKey("rotated-kid", "active", now);
    },
  };
  return store;
}

describe("rotateOidcKeyIfDue (pure step)", () => {
  const now = new Date("2026-08-01T00:00:00Z");
  const policy = { rotationAgeMs: 90 * DAY_MS, publishOverlapMs: 7 * DAY_MS };

  test("creates the active key when none exists", async () => {
    const store = fakeStore([]);
    const outcome = await rotateOidcKeyIfDue(store, now, policy);
    expect(outcome).toEqual({ action: "created", kid: "created-kid" });
    expect(store.rotations).toBe(0);
  });

  test("keeps a young key", async () => {
    const store = fakeStore([fakeKey("young", "active", new Date(now.getTime() - DAY_MS))]);
    const outcome = await rotateOidcKeyIfDue(store, now, policy);
    expect(outcome).toEqual({ action: "current", kid: "young" });
    expect(store.rotations).toBe(0);
    expect(store.creates).toBe(0);
  });

  test("rotates a key at its age limit", async () => {
    const store = fakeStore([fakeKey("old", "active", new Date(now.getTime() - 91 * DAY_MS))]);
    const outcome = await rotateOidcKeyIfDue(store, now, policy);
    expect(outcome).toEqual({ action: "rotated", kid: "rotated-kid" });
    expect(store.rotations).toBe(1);
  });
});

describe("admin rotation route", () => {
  const path = "/api/v1/admin/integrations/oidc/rotate";

  function route(role: string | null) {
    const store = fakeStore([]);
    const app = makeOidcKeyAdminRoute({
      keys: store,
      now: () => new Date("2026-08-01T00:00:00Z"),
      getSession: async () => (role === null ? null : { user: { id: "u1", role } }),
    });
    return { app, store };
  }

  test("rejects unauthenticated and non-admin callers", async () => {
    const anonymous = route(null);
    expect((await anonymous.app.request(path, { method: "POST" })).status).toBe(401);
    expect(anonymous.store.rotations).toBe(0);

    const member = route("user");
    expect((await member.app.request(path, { method: "POST" })).status).toBe(403);
    expect(member.store.rotations).toBe(0);
  });

  test("an admin rotation returns the new kid and the overlap deadline", async () => {
    const admin = route("admin");
    const response = await admin.app.request(path, { method: "POST" });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      kid: "rotated-kid",
      publishOverlapUntil: new Date(
        new Date("2026-08-01T00:00:00Z").getTime() + OIDC_KEY_PUBLISH_OVERLAP_MS,
      ).toISOString(),
    });
    expect(admin.store.rotations).toBe(1);
  });
});

describe("integration_oidc_key store (live PG)", () => {
  test.skipIf(!dbReachable)(
    "JWKS publishes both kids inside the overlap window and drops the retiring kid after it",
    async () => {
      const db = getDb();
      const store = makeIntegrationOidcKeyStore(db);
      const t0 = new Date("2026-08-01T00:00:00Z");

      // Isolate from any pre-existing deployment keys.
      await db.delete(keyTable).where(inArray(keyTable.state, ["active", "retiring"]));
      try {
        const first = await store.getOrCreateActive(t0);
        const overlapMs = 7 * DAY_MS;
        const second = await store.rotate(new Date(t0.getTime() + DAY_MS), overlapMs);
        expect(second.kid).not.toBe(first.kid);

        // Inside the overlap window: BOTH kids serve.
        const inside = await store.listPublished(new Date(t0.getTime() + 2 * DAY_MS));
        expect(new Set(inside.map((key) => key.kid))).toEqual(new Set([first.kid, second.kid]));
        expect(inside.find((key) => key.kid === first.kid)?.state).toBe("retiring");
        expect(inside.find((key) => key.kid === second.kid)?.state).toBe("active");

        // After the overlap window: only the active kid serves.
        const after = await store.listPublished(new Date(t0.getTime() + DAY_MS + overlapMs + 1000));
        expect(after.map((key) => key.kid)).toEqual([second.kid]);
      } finally {
        await db.delete(keyTable).where(inArray(keyTable.state, ["active", "retiring"]));
      }
    },
  );

  test.skipIf(!dbReachable)(
    "concurrent rotation never yields two active keys",
    async () => {
      const db = getDb();
      const store = makeIntegrationOidcKeyStore(db);
      const t0 = new Date("2026-08-01T00:00:00Z");

      await db.delete(keyTable).where(inArray(keyTable.state, ["active", "retiring"]));
      try {
        await store.getOrCreateActive(t0);
        // Two racing rotations: the partial unique index on state='active'
        // lets one insert win; the loser's whole transaction rolls back.
        const results = await Promise.allSettled([
          store.rotate(new Date(t0.getTime() + DAY_MS), 7 * DAY_MS),
          store.rotate(new Date(t0.getTime() + DAY_MS), 7 * DAY_MS),
        ]);
        expect(results.some((result) => result.status === "fulfilled")).toBe(true);

        const activeRows = await db
          .select({ kid: keyTable.kid })
          .from(keyTable)
          .where(eq(keyTable.state, "active"));
        expect(activeRows).toHaveLength(1);

        // getOrCreateActive converges on the surviving active key.
        const active = await store.getOrCreateActive(new Date(t0.getTime() + 2 * DAY_MS));
        expect(active.kid).toBe(activeRows[0]!.kid);
      } finally {
        await db.delete(keyTable).where(inArray(keyTable.state, ["active", "retiring"]));
      }
    },
  );
});
