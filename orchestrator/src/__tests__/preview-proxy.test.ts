/**
 * Live-host preview reverse-proxy (ADR 0064 P2b):
 *   - previewHostLabel host parsing
 *   - authorizePreview matrix (404/410/401/403, owner/admin/share-token)
 *   - end-to-end HTTP proxy over a FAKE PortRelay (validates the
 *     http.request-over-Duplex tunnel mechanics on the real Bun runtime)
 */

import { expect, test, describe } from "bun:test";
import { gzipSync } from "node:zlib";
import { Hono } from "hono";
import { create } from "@bufbuild/protobuf";

import {
  previewHostLabel,
  authorizePreview,
  makePreviewProxyMiddleware,
  type PortRelayClient,
} from "../routes/preview-proxy.ts";
import {
  RelayPortResponseSchema,
  type RelayPortRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import type { SessionAppRow, SessionAppStore } from "../db/session-apps.ts";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function row(overrides: Partial<SessionAppRow> = {}): SessionAppRow {
  return {
    hostLabel: "web-jumping-fat-kittens",
    sessionId: "sess_1",
    name: "web",
    port: 3000,
    ownerUserId: "owner",
    visibility: "org",
    createdAt: new Date(0),
    ...overrides,
  };
}

function fakeStore(seed: SessionAppRow[]): SessionAppStore {
  const byLabel = new Map(seed.map((r) => [r.hostLabel, r]));
  return {
    async createMany() {
      throw new Error("unused");
    },
    async createOne() {
      throw new Error("unused");
    },
    async listBySession() {
      return [];
    },
    async getByHostLabel(label) {
      return byLabel.get(label) ?? null;
    },
    async deleteByHostLabel() {
      return false;
    },
    async deleteBySession() {
      return 0;
    },
  };
}

const noSession = async () => null;
const asUser = (id: string, role?: string) => async () => ({ user: { id, role } });

/** A PortRelay that ignores the request and replies with a canned HTTP/1.1
 * response — i.e. behaves like the guest origin for one round-trip. */
function fakeRelayServing(responseText: string | Uint8Array): PortRelayClient {
  const bytes =
    typeof responseText === "string" ? new TextEncoder().encode(responseText) : responseText;
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

describe("previewHostLabel", () => {
  test("matches dev (with port) and prod (no port) preview hosts", () => {
    expect(previewHostLabel("web-jumping-fat-kittens.lvh.me:8787", "lvh.me:8787")).toBe(
      "web-jumping-fat-kittens",
    );
    expect(
      previewHostLabel(
        "web-jumping-fat-kittens.preview.example.com",
        "preview.example.com",
      ),
    ).toBe("web-jumping-fat-kittens");
  });

  test("rejects apex, multi-level, foreign, and junk-slug hosts", () => {
    expect(previewHostLabel("lvh.me:8787", "lvh.me:8787")).toBeNull(); // apex
    expect(previewHostLabel("a.b.lvh.me:8787", "lvh.me:8787")).toBeNull(); // nested
    expect(previewHostLabel("evil.example.com", "lvh.me:8787")).toBeNull(); // foreign
    expect(previewHostLabel("UPPER.lvh.me:8787", "lvh.me:8787")).toBe("upper"); // lowercased
    expect(previewHostLabel(undefined, "lvh.me:8787")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

describe("authorizePreview", () => {
  const H = new Headers();

  test("404 for an unknown host label", async () => {
    const r = await authorizePreview({
      hostLabel: "nope",
      headers: H,
      store: fakeStore([]),
      getSession: asUser("owner"),
    });
    expect(r).toEqual({ ok: false, status: 404 });
  });

  test("401 when there is no session", async () => {
    const r = await authorizePreview({
      hostLabel: "web-jumping-fat-kittens",
      headers: H,
      store: fakeStore([row()]),
      getSession: noSession,
    });
    expect(r).toEqual({ ok: false, status: 401 });
  });

  test("403 when authenticated but not owner/admin", async () => {
    const r = await authorizePreview({
      hostLabel: "web-jumping-fat-kittens",
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("intruder"),
    });
    expect(r).toEqual({ ok: false, status: 403 });
  });

  test("owner and admin are allowed", async () => {
    const owner = await authorizePreview({
      hostLabel: "web-jumping-fat-kittens",
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("owner"),
    });
    expect(owner.ok).toBe(true);

    const admin = await authorizePreview({
      hostLabel: "web-jumping-fat-kittens",
      headers: H,
      store: fakeStore([row()]),
      getSession: asUser("someone", "admin"),
    });
    expect(admin.ok).toBe(true);
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
    const res = await app.request("http://web-jumping-fat-kittens.lvh.me:8787/index.html", {
      headers: { host: "web-jumping-fat-kittens.lvh.me:8787" },
    });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/plain");
    expect(await res.text()).toBe("hello world");
  });

  test("strips content-encoding/content-length when fetch decompressed the body", async () => {
    // Guest origin gzips its response (as e.g. an Express/Vite dev server does
    // when the browser sends Accept-Encoding). Bun's fetch transparently
    // decompresses the body; forwarding the original headers with the
    // decompressed bytes is exactly net::ERR_CONTENT_DECODING_FAILED.
    const body = gzipSync(Buffer.from("hello gzipped world"));
    const head =
      "HTTP/1.1 200 OK\r\n" +
      "Content-Type: text/html\r\n" +
      "Content-Encoding: gzip\r\n" +
      `Content-Length: ${body.length}\r\n` +
      "\r\n";
    const canned = Buffer.concat([Buffer.from(head), body]);

    const app = appWith(fakeRelayServing(new Uint8Array(canned)));
    const res = await app.request("http://web-jumping-fat-kittens.lvh.me:8787/", {
      headers: {
        host: "web-jumping-fat-kittens.lvh.me:8787",
        "accept-encoding": "gzip, deflate, br",
      },
    });
    expect(res.status).toBe(200);
    // The forwarded headers must describe the (decompressed) body we forward.
    expect(res.headers.get("content-encoding")).toBeNull();
    expect(res.headers.get("content-length")).not.toBe(String(body.length));
    expect(await res.text()).toBe("hello gzipped world");
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
    const res = await app.request("http://web-jumping-fat-kittens.lvh.me:8787/", {
      headers: { host: "web-jumping-fat-kittens.lvh.me:8787" },
    });
    expect(res.status).toBe(401);
  });
});
