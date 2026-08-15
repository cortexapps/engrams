/**
 * Session-app CRUD route (ADR 0118, replacing ADR 0064's ports route):
 *   - 401 unauthenticated; 404 when the caller doesn't own the session
 *   - POST validates the port, derives a name, and is idempotent per session
 *   - GET lists; DELETE revokes (and 404s a label from another session)
 *   - admin (manage:all) may create for a session it doesn't own
 */

import { expect, test, describe } from "bun:test";
import { create } from "@bufbuild/protobuf";

import { makeAppsRoute } from "../routes/apps.ts";
import { RelayPortResponseSchema } from "../gen/engram/app/v1/session_pb.ts";
import type { PortRelayClient } from "../routes/preview-proxy.ts";
import type {
  SessionAppRow,
  SessionAppStore,
} from "../db/session-apps.ts";

function fakeStore(): SessionAppStore & { rows: Map<string, SessionAppRow> } {
  const rows = new Map<string, SessionAppRow>();
  let n = 0;
  const find = (sid: string, name: string, port: number) =>
    [...rows.values()].find(
      (r) => r.sessionId === sid && (r.name === name || r.port === port),
    ) ?? null;
  const store: SessionAppStore & { rows: Map<string, SessionAppRow> } = {
    rows,
    async createMany(sessionId, ownerUserId, apps) {
      const slug = `slug-${++n}`;
      const out: SessionAppRow[] = [];
      for (const a of apps) {
        const existing = find(sessionId, a.name, a.port);
        if (existing) {
          out.push(existing);
          continue;
        }
        const row: SessionAppRow = {
          hostLabel: `${a.name}-${slug}`,
          sessionId,
          name: a.name,
          port: a.port,
          ownerUserId,
          visibility: a.visibility ?? "org",
          createdAt: new Date(0),
        };
        rows.set(row.hostLabel, row);
        out.push(row);
      }
      return out;
    },
    async createOne(sessionId, ownerUserId, app) {
      const [row] = await store.createMany(sessionId, ownerUserId, [app]);
      return row!;
    },
    async listBySession(sid) {
      return [...rows.values()].filter((r) => r.sessionId === sid);
    },
    async getByHostLabel(label) {
      return rows.get(label) ?? null;
    },
    async deleteByHostLabel(label) {
      return rows.delete(label);
    },
    async deleteBySession(sid) {
      let n = 0;
      for (const [k, r] of [...rows]) if (r.sessionId === sid) { rows.delete(k); n++; }
      return n;
    },
  };
  return store;
}

const anon = async () => null;
const asUser = (id: string, role?: string) => async () => ({ user: { id, role } });
const ownedBy = (uid: string) => async () => uid;

const POST = (body: unknown) => ({
  method: "POST",
  body: JSON.stringify(body),
  headers: { "content-type": "application/json" },
});

describe("apps CRUD route", () => {
  test("401 when unauthenticated", async () => {
    const app = makeAppsRoute({ store: fakeStore(), getSession: anon, resolveOwner: ownedBy("u") });
    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }));
    expect(res.status).toBe(401);
  });

  test("404 when the caller does not own the session", async () => {
    const app = makeAppsRoute({
      store: fakeStore(),
      getSession: asUser("intruder"),
      resolveOwner: ownedBy("owner"),
    });
    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }));
    expect(res.status).toBe(404);
  });

  test("POST reserves an app, derives its name, and is idempotent per session", async () => {
    const store = fakeStore();
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      previewBaseDomain: "preview.example.com",
    });

    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000, name: "web" }));
    expect(res.status).toBe(201);
    const body = (await res.json()) as {
      hostLabel: string;
      url: string;
      port: number;
      name: string;
      visibility: string;
    };
    expect(body.port).toBe(3000);
    expect(body.name).toBe("web");
    expect(body.visibility).toBe("org");
    expect(body.url).toBe(`https://${body.hostLabel}.preview.example.com`);

    // Re-reserve the same port → same label, no duplicate row.
    const res2 = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000, name: "web" }));
    const body2 = (await res2.json()) as { hostLabel: string };
    expect(body2.hostLabel).toBe(body.hostLabel);
    expect(store.rows.size).toBe(1);
  });

  test("an ad-hoc reservation with no name is an app named port-<port>", async () => {
    const store = fakeStore();
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      previewBaseDomain: "preview.example.com",
    });
    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }));
    const body = (await res.json()) as { name: string; hostLabel: string };
    expect(body.name).toBe("port-3000");
    expect(body.hostLabel.startsWith("port-3000-")).toBe(true);
  });

  test("POST rejects a name that cannot be a DNS label", async () => {
    const app = makeAppsRoute({
      store: fakeStore(),
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
    });
    for (const name of ["bad name", "has.dot", "-leading", "a".repeat(25)]) {
      const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000, name }));
      expect(res.status).toBe(400);
    }
  });

  test("POST rejects an out-of-range port", async () => {
    const app = makeAppsRoute({
      store: fakeStore(),
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
    });
    for (const port of [0, 70000, 3.5, "x"]) {
      const res = await app.request("/api/v1/sessions/s1/apps", POST({ port }));
      expect(res.status).toBe(400);
    }
  });

  test("dev preview domain yields an http URL", async () => {
    const app = makeAppsRoute({
      store: fakeStore(),
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      previewBaseDomain: "lvh.me:8787",
    });
    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 5173 }));
    const body = (await res.json()) as { url: string; hostLabel: string };
    expect(body.url).toBe(`http://${body.hostLabel}.lvh.me:8787`);
  });

  test("GET lists the session's apps", async () => {
    const store = fakeStore();
    const app = makeAppsRoute({ store, getSession: asUser("owner"), resolveOwner: ownedBy("owner") });
    await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }));
    await app.request("/api/v1/sessions/s1/apps", POST({ port: 8080 }));
    const res = await app.request("/api/v1/sessions/s1/apps");
    expect(res.status).toBe(200);
    const body = (await res.json()) as { apps: unknown[] };
    expect(body.apps.length).toBe(2);
  });

  test("DELETE revokes an owned app; 404 for a label from another session", async () => {
    const store = fakeStore();
    const app = makeAppsRoute({ store, getSession: asUser("owner"), resolveOwner: ownedBy("owner") });
    const created = (await (
      await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }))
    ).json()) as { hostLabel: string };

    // Wrong session → 404, row untouched.
    const wrong = await app.request(`/api/v1/sessions/s2/apps/${created.hostLabel}`, { method: "DELETE" });
    expect(wrong.status).toBe(404);
    expect(store.rows.size).toBe(1);

    // Right session → 204, row gone.
    const ok = await app.request(`/api/v1/sessions/s1/apps/${created.hostLabel}`, { method: "DELETE" });
    expect(ok.status).toBe(204);
    expect(store.rows.size).toBe(0);
  });

  test("admin may reserve an app on a session it does not own", async () => {
    const store = fakeStore();
    const app = makeAppsRoute({
      store,
      getSession: asUser("the-admin", "admin"),
      resolveOwner: ownedBy("someone-else"),
    });
    const res = await app.request("/api/v1/sessions/s1/apps", POST({ port: 3000 }));
    expect(res.status).toBe(201);
  });
});

// ---------------------------------------------------------------------------
// Liveness probe (GET …/apps/:slug/health)
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
const seed = (store: SessionAppStore, sessionId = "s1", port = 3000) =>
  store.createOne(sessionId, "owner", { name: `port-${port}`, port });

describe("apps liveness route", () => {
  test("active + port answers → up", async () => {
    const store = fakeStore();
    const row = await seed(store);
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("up"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s1/apps/${row.hostLabel}/health`);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ status: "up" });
  });

  test("active + port silent/closed → down", async () => {
    const store = fakeStore();
    const row = await seed(store);
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("down"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s1/apps/${row.hostLabel}/health`);
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
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: spyRelay,
      sessionStatus: async () => "idle",
    });
    const res = await app.request(`/api/v1/sessions/s1/apps/${row.hostLabel}/health`);
    expect(await res.json()).toEqual({ status: "unknown" });
    expect(dialed).toBe(false);
  });

  test("404 for a slug that is not on this session", async () => {
    const store = fakeStore();
    const row = await seed(store, "s1");
    const app = makeAppsRoute({
      store,
      getSession: asUser("owner"),
      resolveOwner: ownedBy("owner"),
      portRelay: fakeRelay("up"),
      sessionStatus: active,
    });
    const res = await app.request(`/api/v1/sessions/s2/apps/${row.hostLabel}/health`);
    expect(res.status).toBe(404);
  });
});
