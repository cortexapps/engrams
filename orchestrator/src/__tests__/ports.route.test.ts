/**
 * Port-exposure CRUD route (ADR 0064 P2a):
 *   - 401 unauthenticated; 404 when the caller doesn't own the session
 *   - POST validates the port + is idempotent per (session, port)
 *   - GET lists; DELETE revokes (and 404s a slug from another session)
 *   - admin (manage:all) may create for a session it doesn't own
 */

import { expect, test, describe } from "bun:test";
import { create } from "@bufbuild/protobuf";

import { makePortsRoute } from "../routes/ports.ts";
import { RelayPortResponseSchema } from "../gen/engram/app/v1/session_pb.ts";
import type { PortRelayClient } from "../routes/preview-proxy.ts";
import type {
  PortExposureRow,
  PortExposureStore,
} from "../db/port-exposures.ts";

function fakeStore(): PortExposureStore & { rows: Map<string, PortExposureRow> } {
  const rows = new Map<string, PortExposureRow>();
  let n = 0;
  const findBySessionPort = (sid: string, port: number) =>
    [...rows.values()].find((r) => r.sessionId === sid && r.port === port) ?? null;
  return {
    rows,
    async createOrGet(input) {
      const existing = findBySessionPort(input.sessionId, input.port);
      if (existing) return existing;
      const slug = `slug-${++n}`;
      const row: PortExposureRow = {
        slug,
        sessionId: input.sessionId,
        port: input.port,
        label: input.label,
        ownerUserId: input.ownerUserId,
        visibility: input.visibility,
        shareToken: input.visibility === "shared" ? "tok-xyz" : null,
        createdAt: new Date(0),
        expiresAt: null,
      };
      rows.set(slug, row);
      return row;
    },
    async listBySession(sid) {
      return [...rows.values()].filter((r) => r.sessionId === sid);
    },
    async getBySlug(slug) {
      return rows.get(slug) ?? null;
    },
    async deleteBySlug(slug) {
      return rows.delete(slug);
    },
  };
}

const anon = async () => null;
const asUser = (id: string, role?: string) => async () => ({ user: { id, role } });
const ownedBy = (uid: string) => async () => uid;

const POST = (body: unknown) => ({
  method: "POST",
  body: JSON.stringify(body),
  headers: { "content-type": "application/json" },
});

describe("ports CRUD route", () => {
  test("401 when unauthenticated", async () => {
    const app = makePortsRoute({ store: fakeStore(), getSession: anon, resolveOwner: ownedBy("u") });
    const res = await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }));
    expect(res.status).toBe(401);
  });

  test("404 when the caller does not own the session", async () => {
    const app = makePortsRoute({
      store: fakeStore(),
      getSession: asUser("intruder"),
      resolveOwner: ownedBy("owner"),
    });
    const res = await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }));
    expect(res.status).toBe(404);
  });

  test("POST creates an exposure and is idempotent per (session, port)", async () => {
    const store = fakeStore();
    const app = makePortsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      previewBaseDomain: "preview.example.com",
    });

    const res = await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000, label: "Vite" }));
    expect(res.status).toBe(201);
    const body = (await res.json()) as { slug: string; url: string; port: number; label: string };
    expect(body.port).toBe(3000);
    expect(body.label).toBe("Vite");
    expect(body.url).toBe(`https://${body.slug}.preview.example.com`);

    // Re-expose the same port → same slug, no duplicate row.
    const res2 = await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }));
    const body2 = (await res2.json()) as { slug: string };
    expect(body2.slug).toBe(body.slug);
    expect(store.rows.size).toBe(1);
  });

  test("POST rejects an out-of-range port", async () => {
    const app = makePortsRoute({
      store: fakeStore(),
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
    });
    for (const port of [0, 70000, 3.5, "x"]) {
      const res = await app.request("/api/v1/sessions/s1/ports", POST({ port }));
      expect(res.status).toBe(400);
    }
  });

  test("dev preview domain yields an http URL", async () => {
    const app = makePortsRoute({
      store: fakeStore(),
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      previewBaseDomain: "lvh.me:8787",
    });
    const res = await app.request("/api/v1/sessions/s1/ports", POST({ port: 5173 }));
    const body = (await res.json()) as { url: string; slug: string };
    expect(body.url).toBe(`http://${body.slug}.lvh.me:8787`);
  });

  test("GET lists the session's exposures", async () => {
    const store = fakeStore();
    const app = makePortsRoute({ store, getSession: asUser("owner"), resolveOwner: ownedBy("owner") });
    await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }));
    await app.request("/api/v1/sessions/s1/ports", POST({ port: 8080 }));
    const res = await app.request("/api/v1/sessions/s1/ports");
    expect(res.status).toBe(200);
    const body = (await res.json()) as { exposures: unknown[] };
    expect(body.exposures.length).toBe(2);
  });

  test("DELETE revokes an owned exposure; 404 for a slug from another session", async () => {
    const store = fakeStore();
    const app = makePortsRoute({ store, getSession: asUser("owner"), resolveOwner: ownedBy("owner") });
    const created = (await (
      await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }))
    ).json()) as { slug: string };

    // Wrong session → 404, row untouched.
    const wrong = await app.request(`/api/v1/sessions/s2/ports/${created.slug}`, { method: "DELETE" });
    expect(wrong.status).toBe(404);
    expect(store.rows.size).toBe(1);

    // Right session → 204, row gone.
    const ok = await app.request(`/api/v1/sessions/s1/ports/${created.slug}`, { method: "DELETE" });
    expect(ok.status).toBe(204);
    expect(store.rows.size).toBe(0);
  });

  test("admin may expose a port on a session it does not own", async () => {
    const store = fakeStore();
    const app = makePortsRoute({
      store,
      getSession: asUser("the-admin", "admin"),
      resolveOwner: ownedBy("someone-else"),
    });
    const res = await app.request("/api/v1/sessions/s1/ports", POST({ port: 3000 }));
    expect(res.status).toBe(201);
  });
});

// ---------------------------------------------------------------------------
// Liveness probe (GET …/ports/:slug/health)
// ---------------------------------------------------------------------------

/** A PortRelay that simulates an answering ("up") or closed ("down") guest
 * port: it drains the probe's outbound (open + HEAD) and yields either a data
 * frame (HTTP bytes back) or an immediate close. */
function fakeRelay(mode: "up" | "down"): PortRelayClient {
  return {
    relay(inbound) {
      return (async function* () {
        void (async () => {
          try {
            for await (const _ of inbound) {
              /* consume open + HEAD so the probe's writes don't block */
            }
          } catch {
            /* aborted on settle */
          }
        })();
        if (mode === "up") {
          yield create(RelayPortResponseSchema, {
            frame: { case: "data", value: new TextEncoder().encode("HTTP/1.0 200 OK\r\n\r\n") },
          });
        } else {
          yield create(RelayPortResponseSchema, { frame: { case: "close", value: {} } });
        }
      })();
    },
  };
}

const active = async () => "active";
const seed = (store: PortExposureStore, sessionId = "s1", port = 3000) =>
  store.createOrGet({ sessionId, port, label: "", ownerUserId: "owner", visibility: "private" });

describe("ports liveness route", () => {
  test("active + port answers → up", async () => {
    const store = fakeStore();
    const row = await seed(store);
    const app = makePortsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("up"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s1/ports/${row.slug}/health`);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ status: "up" });
  });

  test("active + port silent/closed → down", async () => {
    const store = fakeStore();
    const row = await seed(store);
    const app = makePortsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("down"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s1/ports/${row.slug}/health`);
    expect(await res.json()).toEqual({ status: "down" });
  });

  test("non-active session → unknown WITHOUT dialing the relay (never wakes a VM)", async () => {
    const store = fakeStore();
    const row = await seed(store);
    let dialed = false;
    const spyRelay: PortRelayClient = {
      relay() {
        dialed = true;
        return (async function* () {})();
      },
    };
    const app = makePortsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: spyRelay,
      sessionStatus: async () => "idle",
    });
    const res = await app.request(`/api/v1/sessions/s1/ports/${row.slug}/health`);
    expect(await res.json()).toEqual({ status: "unknown" });
    expect(dialed).toBe(false);
  });

  test("404 for a slug that is not on this session", async () => {
    const store = fakeStore();
    const row = await seed(store, "s1");
    const app = makePortsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("up"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s2/ports/${row.slug}/health`);
    expect(res.status).toBe(404);
  });
});
