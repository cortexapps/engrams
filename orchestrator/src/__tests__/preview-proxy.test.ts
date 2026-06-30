/**
 * Live-host preview reverse-proxy (ADR 0064 P2b):
 *   - previewSlugFromHost host parsing
 *   - authorizePreview matrix (404/410/401/403, owner/admin/share-token)
 *   - end-to-end HTTP proxy over a FAKE PortRelay (validates the
 *     http.request-over-Duplex tunnel mechanics on the real Bun runtime)
 */

import { expect, test, describe } from "bun:test";
import { Hono } from "hono";
import { create } from "@bufbuild/protobuf";

import {
  previewSlugFromHost,
  authorizePreview,
  makePreviewProxyMiddleware,
  type PortRelayClient,
} from "../routes/preview-proxy.ts";
import {
  RelayPortResponseSchema,
  type RelayPortRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import type { PortExposureRow, PortExposureStore } from "../db/port-exposures.ts";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function row(overrides: Partial<PortExposureRow> = {}): PortExposureRow {
  return {
    slug: "jumping-fat-kittens",
    sessionId: "sess_1",
    port: 3000,
    label: "",
    ownerUserId: "owner",
    visibility: "private",
    shareToken: null,
    createdAt: new Date(0),
    expiresAt: null,
    ...overrides,
  };
}

function fakeStore(seed: PortExposureRow[]): PortExposureStore {
  const bySlug = new Map(seed.map((r) => [r.slug, r]));
  return {
    async createOrGet() {
      throw new Error("unused");
    },
    async listBySession() {
      return [];
    },
    async getBySlug(slug) {
      return bySlug.get(slug) ?? null;
    },
    async deleteBySlug() {
      return false;
    },
  };
}

const noSession = async () => null;
const asUser = (id: string, role?: string) => async () => ({ user: { id, role } });

/** A PortRelay that ignores the request and replies with a canned HTTP/1.1
 * response — i.e. behaves like the guest origin for one round-trip. */
function fakeRelayServing(responseText: string): PortRelayClient {
  const bytes = new TextEncoder().encode(responseText);
  return {
    relay(inbound: AsyncIterable<RelayPortRequest>) {
      return (async function* () {
        // Drain the request frames in the background so the Duplex write side
        // never blocks on a full channel.
        void (async () => {
          for await (const _ of inbound) {
            /* discard */
          }
        })();
        yield create(RelayPortResponseSchema, { frame: { case: "data", value: bytes } });
      })();
    },
  };
}

const CANNED_200 =
  "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nhello world";

// ---------------------------------------------------------------------------
// Host parsing
// ---------------------------------------------------------------------------

describe("previewSlugFromHost", () => {
  test("matches dev (with port) and prod (no port) preview hosts", () => {
    expect(previewSlugFromHost("jumping-fat-kittens.lvh.me:8787", "lvh.me:8787")).toBe(
      "jumping-fat-kittens",
    );
    expect(
      previewSlugFromHost(
        "jumping-fat-kittens.preview.engrams.cortex.io",
        "preview.engrams.cortex.io",
      ),
    ).toBe("jumping-fat-kittens");
  });

  test("rejects apex, multi-level, foreign, and junk-slug hosts", () => {
    expect(previewSlugFromHost("lvh.me:8787", "lvh.me:8787")).toBeNull(); // apex
    expect(previewSlugFromHost("a.b.lvh.me:8787", "lvh.me:8787")).toBeNull(); // nested
    expect(previewSlugFromHost("evil.example.com", "lvh.me:8787")).toBeNull(); // foreign
    expect(previewSlugFromHost("UPPER.lvh.me:8787", "lvh.me:8787")).toBe("upper"); // lowercased
    expect(previewSlugFromHost(undefined, "lvh.me:8787")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

describe("authorizePreview", () => {
  const H = new Headers();

  test("404 for an unknown slug", async () => {
    const r = await authorizePreview({
      slug: "nope",
      token: null,
      headers: H,
      store: fakeStore([]),
      getSession: asUser("owner"),
    });
    expect(r).toEqual({ ok: false, status: 404 });
  });

  test("410 for an expired exposure", async () => {
    const r = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: null,
      headers: H,
      store: fakeStore([row({ expiresAt: new Date(1000) })]),
      getSession: asUser("owner"),
      now: new Date(2000),
    });
    expect(r).toEqual({ ok: false, status: 410 });
  });

  test("401 when private + no session", async () => {
    const r = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: null,
      headers: H,
      store: fakeStore([row()]),
      getSession: noSession,
    });
    expect(r).toEqual({ ok: false, status: 401 });
  });

  test("403 when authenticated but not owner/admin", async () => {
    const r = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: null,
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("intruder"),
    });
    expect(r).toEqual({ ok: false, status: 403 });
  });

  test("owner and admin are allowed", async () => {
    const owner = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: null,
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("owner"),
    });
    expect(owner.ok).toBe(true);

    const admin = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: null,
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("someone", "admin"),
    });
    expect(admin.ok).toBe(true);
  });

  test("a valid share-token grants access without a session; a wrong one falls through", async () => {
    const shared = row({ visibility: "shared", shareToken: "secret-token" });
    const ok = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: "secret-token",
      headers: H,
      store: fakeStore([shared]),
      getSession: noSession,
    });
    expect(ok.ok).toBe(true);

    const wrong = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: "wrong",
      headers: H,
      store: fakeStore([shared]),
      getSession: noSession,
    });
    expect(wrong).toEqual({ ok: false, status: 401 }); // falls through to session check

    // A token is ignored for a private exposure.
    const priv = await authorizePreview({
      slug: "jumping-fat-kittens",
      token: "secret-token",
      headers: H,
      store: fakeStore([row({ shareToken: "secret-token" })]), // visibility=private
      getSession: noSession,
    });
    expect(priv).toEqual({ ok: false, status: 401 });
  });
});

// ---------------------------------------------------------------------------
// End-to-end HTTP proxy (validates tunnelSocket + http.request on Bun)
// ---------------------------------------------------------------------------

describe("preview proxy middleware (HTTP)", () => {
  function appWith(relay: PortRelayClient) {
    const app = new Hono();
    app.use(
      makePreviewProxyMiddleware({
        store: fakeStore([row()]),
        portRelay: relay,
        getSession: asUser("owner"),
        previewBaseDomain: "lvh.me:8787",
      }),
    );
    app.get("*", (c) => c.text("APP", 200)); // non-preview fallthrough
    return app;
  }

  test("proxies a preview host to the guest response over the tunnel", async () => {
    const app = appWith(fakeRelayServing(CANNED_200));
    const res = await app.request("http://jumping-fat-kittens.lvh.me:8787/index.html", {
      headers: { host: "jumping-fat-kittens.lvh.me:8787" },
    });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/plain");
    expect(await res.text()).toBe("hello world");
  });

  test("non-preview host falls through to the app", async () => {
    const app = appWith(fakeRelayServing(CANNED_200));
    const res = await app.request("http://localhost:8787/whatever", {
      headers: { host: "localhost:8787" },
    });
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("APP");
  });

  test("unauthorized preview request is rejected before proxying", async () => {
    const app = new Hono();
    app.use(
      makePreviewProxyMiddleware({
        store: fakeStore([row()]),
        portRelay: fakeRelayServing(CANNED_200),
        getSession: noSession,
        previewBaseDomain: "lvh.me:8787",
      }),
    );
    const res = await app.request("http://jumping-fat-kittens.lvh.me:8787/", {
      headers: { host: "jumping-fat-kittens.lvh.me:8787" },
    });
    expect(res.status).toBe(401);
  });
});
