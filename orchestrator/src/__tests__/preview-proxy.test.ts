/**
 * Session-app preview reverse-proxy (ADR 0064 P2b, wall per ADR 0118):
 *   - previewHostLabel host parsing
 *   - authorizeApp matrix (401/403, org vs private visibility)
 *   - the four ADR 0118 invariants: termination, unauthenticated OPTIONS,
 *     sibling-origin enforcement, and navigation-vs-XHR refusal shape
 *   - end-to-end HTTP proxy over a FAKE PortRelay (validates the
 *     http.request-over-Duplex tunnel mechanics on the real Bun runtime)
 */

import { expect, test, describe } from "bun:test";
import { gzipSync } from "node:zlib";
import { Hono } from "hono";
import { create } from "@bufbuild/protobuf";

import {
  previewHostLabel,
  authorizeApp,
  sanitizeGuestSetCookie,
  stripOrchestratorCredentials,
  isSiblingOrigin,
  isUnderPreviewDomain,
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

describe("authorizeApp", () => {
  const H = new Headers();

  test("401 when there is no session", async () => {
    const r = await authorizeApp({ row: row(), headers: H, getSession: noSession });
    expect(r).toEqual({ ok: false, status: 401 });
  });

  test('visibility "org" admits any authenticated principal', async () => {
    // The default. A teammate can open your preview by URL — they are past the
    // login wall, which is the property that matters.
    const r = await authorizeApp({
      row: row({ visibility: "org" }),
      headers: H,
      getSession: asUser("some-teammate"),
    });
    expect(r.ok).toBe(true);
  });

  test('visibility "private" narrows to owner and admin', async () => {
    const priv = row({ visibility: "private" });
    expect(
      (await authorizeApp({ row: priv, headers: H, getSession: asUser("intruder") })).ok,
    ).toBe(false);
    expect(
      (await authorizeApp({ row: priv, headers: H, getSession: asUser("owner") })).ok,
    ).toBe(true);
    expect(
      (await authorizeApp({ row: priv, headers: H, getSession: asUser("x", "admin") })).ok,
    ).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Termination + sibling-origin (the ADR 0118 invariants)
// ---------------------------------------------------------------------------

describe("isUnderPreviewDomain", () => {
  // Everything this matches MUST be answered by the preview handler. It is
  // deliberately wider than previewHostLabel: the apex and a nested label name
  // no app, and are exactly the cases a fall-through would leak.
  test("matches the apex and every label under it, routable or not", () => {
    for (const h of [
      "lvh.me:8787",
      "web-x.lvh.me:8787",
      "a.b.lvh.me:8787",
      "NOT-A-REAL-APP.lvh.me:8787",
    ]) {
      expect(isUnderPreviewDomain(h, "lvh.me:8787")).toBe(true);
    }
  });

  test("does not match a foreign host or a suffix near-miss", () => {
    expect(isUnderPreviewDomain("evil.example.com", "lvh.me:8787")).toBe(false);
    expect(isUnderPreviewDomain("evillvh.me:8787", "lvh.me:8787")).toBe(false);
    expect(isUnderPreviewDomain(undefined, "lvh.me:8787")).toBe(false);
    expect(isUnderPreviewDomain("web-x.lvh.me:8787", "")).toBe(false);
  });
});

describe("isSiblingOrigin", () => {
  const target = row({ hostLabel: "api-x", sessionId: "sess_1", name: "api" });
  const sibling = row({ hostLabel: "web-x", sessionId: "sess_1", name: "web" });
  const stranger = row({ hostLabel: "web-y", sessionId: "sess_2", name: "web" });
  const store = fakeStore([target, sibling, stranger]);
  const base = "lvh.me:8787";

  test("the app itself and a same-session peer are siblings", async () => {
    expect(await isSiblingOrigin("http://api-x.lvh.me:8787", target, base, store)).toBe(true);
    expect(await isSiblingOrigin("http://web-x.lvh.me:8787", target, base, store)).toBe(true);
  });

  test("ANOTHER SESSION's app is not a sibling, even under the same domain", async () => {
    // This is the containment the parent-domain cookie needs: without it, one
    // session's page could issue credentialed requests to another's.
    expect(await isSiblingOrigin("http://web-y.lvh.me:8787", target, base, store)).toBe(false);
  });

  test("a foreign or unparseable origin is never a sibling", async () => {
    expect(await isSiblingOrigin("https://evil.example.com", target, base, store)).toBe(false);
    expect(await isSiblingOrigin("null", target, base, store)).toBe(false);
    expect(await isSiblingOrigin("http://lvh.me:8787", target, base, store)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// End-to-end HTTP proxy (validates tunnelSocket + http.request on Bun)
// ---------------------------------------------------------------------------

describe("preview proxy middleware (HTTP)", () => {
  function appWith(
    relay: PortRelayClient,
    over: Partial<Parameters<typeof makePreviewProxyMiddleware>[0]> = {},
  ) {
    const app = new Hono();
    app.use(
      makePreviewProxyMiddleware({
        store: fakeStore([row()]),
        portRelay: relay,
        getSession: asUser("owner"),
        previewBaseDomain: "lvh.me:8787",
        loginUrl: "https://app.example.com/login",
        ...over,
      }),
    );
    // The fallthrough app. Reaching it from a preview host is the bug the
    // termination invariant exists to prevent, so every test below that
    // expects a refusal also proves this was NOT reached.
    app.all("*", (c) => c.text("APP", 200));
    return app;
  }

  const previewReq = (path = "/", init: RequestInit = {}, host = "web-jumping-fat-kittens.lvh.me:8787") =>
    [`http://${host}${path}`, { ...init, headers: { host, ...(init.headers ?? {}) } }] as const;

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

// ---------------------------------------------------------------------------
// ADR 0118 invariants, at the middleware level
// ---------------------------------------------------------------------------

describe("ADR 0118 invariants", () => {
  function appWith(
    over: Partial<Parameters<typeof makePreviewProxyMiddleware>[0]> = {},
  ) {
    const app = new Hono();
    app.use(
      makePreviewProxyMiddleware({
        store: fakeStore([
          row(),
          row({ hostLabel: "api-jumping-fat-kittens", name: "api", port: 8080 }),
          row({ hostLabel: "web-other", sessionId: "sess_2", name: "web" }),
        ]),
        portRelay: fakeRelayServing(CANNED_200),
        getSession: asUser("owner"),
        previewBaseDomain: "lvh.me:8787",
        loginUrl: "https://app.example.com/login",
        ...over,
      }),
    );
    app.all("*", (c) => c.text("APP", 200));
    return app;
  }

  const HOST = "web-jumping-fat-kittens.lvh.me:8787";

  describe("termination", () => {
    // With the preview domain exempt from the IAP bridge, a fall-through here
    // hands an UNAUTHENTICATED request to the whole orchestrator API.
    test.each([
      ["the apex", "lvh.me:8787"],
      ["a nested label", "a.b.lvh.me:8787"],
      ["a junk label", "not.a.label.lvh.me:8787"],
      ["an unknown app", "no-such-app.lvh.me:8787"],
    ])("%s is answered 404 here, never passed to the app", async (_desc, host) => {
      const res = await appWith().request(`http://${host}/whatever`, { headers: { host } });
      expect(res.status).toBe(404);
      expect(await res.text()).not.toBe("APP");
    });

    test("a host outside the preview domain still reaches the app", async () => {
      const res = await appWith().request("http://app.example.com/x", {
        headers: { host: "app.example.com" },
      });
      expect(await res.text()).toBe("APP");
    });
  });

  describe("a genuine preflight skips the wall", () => {
    // A browser never sends cookies on a preflight, so authenticating it would
    // reject every cross-app call and no CORS config in the app could fix it.
    const PREFLIGHT = {
      host: HOST,
      origin: "http://api-jumping-fat-kittens.lvh.me:8787",
      "access-control-request-method": "POST",
    };

    test("an unauthenticated preflight from a sibling reaches the guest", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
        method: "OPTIONS",
        headers: PREFLIGHT,
      });
      expect(res.status).toBe(200); // reached the guest, not the wall
    });

    test("but a real request on the same path is still walled", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
        method: "GET",
        headers: { host: HOST },
      });
      expect(res.status).toBe(401);
    });

    // `proxyHttp` opens a real relay tunnel and those are capped per session,
    // so an OPTIONS that skipped every check would be an anonymous
    // tunnel-open primitive: a flood exhausts the cap and 503s the session's
    // real users, and it would reach even a private app.
    test("an OPTIONS that is not a preflight is walled like any other request", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
        method: "OPTIONS",
        headers: { host: HOST }, // no Origin, no Access-Control-Request-Method
      });
      expect(res.status).toBe(401);
    });

    test("a preflight missing Access-Control-Request-Method is walled", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
        method: "OPTIONS",
        headers: { host: HOST, origin: "http://api-jumping-fat-kittens.lvh.me:8787" },
      });
      expect(res.status).toBe(401);
    });

    test("a preflight from a NON-sibling origin is refused without a tunnel", async () => {
      for (const origin of ["http://web-other.lvh.me:8787", "https://evil.example.com"]) {
        const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
          method: "OPTIONS",
          headers: { ...PREFLIGHT, origin },
        });
        expect(res.status).toBe(403);
      }
    });
  });

  describe("sibling-origin enforcement", () => {
    test("a same-session sibling is allowed through", async () => {
      const res = await appWith().request(`http://${HOST}/api`, {
        headers: { host: HOST, origin: "http://api-jumping-fat-kittens.lvh.me:8787" },
      });
      expect(res.status).toBe(200);
    });

    test("ANOTHER session's app is refused even with a valid session", async () => {
      // CORS would only stop it reading the response; the request itself would
      // still land on the guest. The refusal has to happen here.
      const res = await appWith().request(`http://${HOST}/api`, {
        headers: { host: HOST, origin: "http://web-other.lvh.me:8787" },
      });
      expect(res.status).toBe(403);
    });

    test("a foreign origin is refused", async () => {
      const res = await appWith().request(`http://${HOST}/api`, {
        headers: { host: HOST, origin: "https://evil.example.com" },
      });
      expect(res.status).toBe(403);
    });

    test("a same-origin request (no Origin header) is unaffected", async () => {
      const res = await appWith().request(`http://${HOST}/index.html`, {
        headers: { host: HOST },
      });
      expect(res.status).toBe(200);
    });
  });

  describe("refusal shape", () => {
    test("an unauthenticated NAVIGATION redirects to login with a next param", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/page`, {
        headers: { host: HOST, accept: "text/html", "sec-fetch-mode": "navigate" },
      });
      expect(res.status).toBe(302);
      const loc = new URL(res.headers.get("location")!);
      expect(loc.origin + loc.pathname).toBe("https://app.example.com/login");
      expect(loc.searchParams.get("next")).toBe(`http://${HOST}/page`);
    });

    // `c.req.url`'s scheme is the SOCKET's, which is plain http behind a
    // TLS-terminating load balancer — the production topology. Sending the user
    // back to an http:// app URL after login is an insecure hop, and fails
    // outright where the LB serves only 443.
    test("next carries the PUBLIC scheme, not the socket's", async () => {
      const res = await appWith({
        getSession: noSession,
        previewBaseDomain: "preview.example.com",
        store: fakeStore([row({ hostLabel: "web-x" })]),
      }).request("http://web-x.preview.example.com/page", {
        headers: {
          host: "web-x.preview.example.com",
          accept: "text/html",
          "sec-fetch-mode": "navigate",
        },
      });
      expect(res.status).toBe(302);
      const next = new URL(res.headers.get("location")!).searchParams.get("next")!;
      expect(next.startsWith("https://")).toBe(true);
    });

    test("next honours X-Forwarded-Proto when the edge sets it", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/page`, {
        headers: {
          host: HOST,
          accept: "text/html",
          "sec-fetch-mode": "navigate",
          "x-forwarded-proto": "https",
        },
      });
      const next = new URL(res.headers.get("location")!).searchParams.get("next")!;
      expect(next.startsWith("https://")).toBe(true);
    });

    test("an unauthenticated XHR gets 401, never a redirect into an HTML page", async () => {
      const res = await appWith({ getSession: noSession }).request(`http://${HOST}/api`, {
        headers: { host: HOST, accept: "application/json", "sec-fetch-mode": "cors" },
      });
      expect(res.status).toBe(401);
    });
  });
});

// ---------------------------------------------------------------------------
// The guest trust boundary (review findings on #1268)
// ---------------------------------------------------------------------------

describe("stripOrchestratorCredentials", () => {
  // The guest runs agent-authored code. Once the session cookie is scoped to
  // the shared parent domain the browser attaches it to every preview request,
  // and forwarding it would hand a visitor's session token to whoever wrote the
  // app — replayable against the main host as that visitor.
  test("removes the orchestrator session cookie, keeps the app's own", () => {
    const h = stripOrchestratorCredentials(
      new Headers({
        cookie: "app_session=keep-me; better-auth.session_token=tok.HMAC; theme=dark",
      }),
    );
    expect(h.get("cookie")).toBe("app_session=keep-me; theme=dark");
  });

  test("covers the __Secure- prefixed variant used over HTTPS", () => {
    const h = stripOrchestratorCredentials(
      new Headers({ cookie: "__Secure-better-auth.session_token=tok; a=1" }),
    );
    expect(h.get("cookie")).toBe("a=1");
  });

  test("drops the Cookie header entirely when nothing survives", () => {
    const h = stripOrchestratorCredentials(
      new Headers({ cookie: "better-auth.session_token=tok" }),
    );
    expect(h.has("cookie")).toBe(false);
  });

  test("removes every header that credentials the ORCHESTRATOR", () => {
    const h = stripOrchestratorCredentials(
      new Headers({
        authorization: "Bearer engk_secret",
        "x-api-key": "engk_secret",
        "x-goog-iap-jwt-assertion": "jwt",
        "x-app-header": "kept",
      }),
    );
    expect(h.has("authorization")).toBe(false);
    expect(h.has("x-api-key")).toBe(false);
    expect(h.has("x-goog-iap-jwt-assertion")).toBe(false);
    expect(h.get("x-app-header")).toBe("kept");
  });
});

describe("sanitizeGuestSetCookie", () => {
  test("drops a guest cookie named like the orchestrator's session", () => {
    // Otherwise a guest could overwrite the visitor's real session across the
    // shared domain — fixation, or denial of their main-host session.
    expect(
      sanitizeGuestSetCookie([
        "better-auth.session_token=attacker; Domain=.example.com; Path=/",
      ]),
    ).toEqual([]);
  });

  test("strips Domain so a guest cookie can only ever be host-only", () => {
    // A guest's app lives at exactly one hostname, so Domain= is never
    // legitimate and would make the cookie readable by every sibling app and
    // by the main host.
    expect(
      sanitizeGuestSetCookie(["SESSION=abc; Domain=.example.com; Path=/; HttpOnly"]),
    ).toEqual(["SESSION=abc; Path=/; HttpOnly"]);
  });

  test("leaves a well-behaved host-only app cookie untouched", () => {
    // The case that has to keep working: an app setting its own session cookie.
    expect(sanitizeGuestSetCookie(["SESSION=abc; Path=/; HttpOnly; SameSite=Lax"])).toEqual([
      "SESSION=abc; Path=/; HttpOnly; SameSite=Lax",
    ]);
  });
});
