/**
 * Live-host preview reverse-proxy (ADR 0064 P2b) — HTTP.
 *
 * A request to `<slug>.<previewBaseDomain>` is proxied to the guest's
 * `localhost:<port>` over the `PortRelayService` raw-byte tunnel:
 *
 *   browser → LB (TLS) → orchestrator [this middleware]
 *     → PortRelayService.Relay (coordinator → host → guest:port)
 *
 * The orchestrator speaks HTTP over the opaque byte tunnel via a custom
 * `http.request({ createConnection })` whose connection is a Duplex backed by
 * the bidi relay stream — so the guest's HTTP/1.1 framing rides the tunnel
 * unchanged.
 *
 * Auth (owner-by-default + share-link + admin-sees-all): the slug resolves to a
 * `(session, port, owner, visibility, shareToken)` row; access requires the
 * authenticated principal to own the session, OR be an admin, OR present the
 * row's `shareToken` (visibility = "shared"). On the internal deployment IAP
 * still fronts everything (caller is already an org member); this adds the
 * per-exposure check.
 *
 * WebSocket upgrades are handled separately (P2b-ws) — Bun's node:http `upgrade`
 * handler can't write to the raw socket (see server.ts), so raw WS passthrough
 * needs its own approach. Plain HTTP (page loads, assets, API, SSE) works here.
 *
 * Deps are injectable for tests; real singletons are used when omitted.
 */

import { timingSafeEqual } from "node:crypto";
import net from "node:net";
import { Duplex } from "node:stream";
import type { Context, MiddlewareHandler } from "hono";
import { create } from "@bufbuild/protobuf";
import {
  RelayPortRequestSchema,
  type RelayPortRequest,
  type RelayPortResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import { portRelay as defaultPortRelay } from "../control-plane/client.ts";
import { config } from "../config.ts";
import { isValidSlug } from "../ports/slug.ts";
import {
  makePortExposureStore,
  type PortExposureRow,
  type PortExposureStore,
} from "../db/port-exposures.ts";
import { pushableQueue } from "./shell.ts";
import type { GetSession } from "./guard.ts";

// ---------------------------------------------------------------------------
// PortRelay client — minimal interface (bidi: AsyncIterable in/out)
// ---------------------------------------------------------------------------

export interface PortRelayClient {
  relay(
    inbound: AsyncIterable<RelayPortRequest>,
    options?: { signal?: AbortSignal },
  ): AsyncIterable<RelayPortResponse>;
}

export interface PreviewProxyDeps {
  store?: PortExposureStore;
  portRelay?: PortRelayClient;
  getSession?: GetSession;
  previewBaseDomain?: string;
}

// ---------------------------------------------------------------------------
// Host → slug parsing (pure)
// ---------------------------------------------------------------------------

/**
 * If `hostHeader` is `<slug>.<baseDomain>` for a single valid slug label,
 * return the slug; otherwise null (apex, multi-level, or foreign host).
 * `baseDomain` carries its port in dev (`lvh.me:8787`) and none in prod
 * (`preview.engrams.cortex.io`, behind the :443 LB) — we compare verbatim.
 */
export function previewSlugFromHost(
  hostHeader: string | undefined,
  baseDomain: string,
): string | null {
  if (!hostHeader) return null;
  const host = hostHeader.toLowerCase();
  const suffix = "." + baseDomain.toLowerCase();
  if (!host.endsWith(suffix)) return null;
  const slug = host.slice(0, -suffix.length);
  // Must be a single DNS label (no nested subdomain) and slug-shaped.
  if (slug.includes(".") || !isValidSlug(slug)) return null;
  return slug;
}

// ---------------------------------------------------------------------------
// Authorization (pure-ish: deps injected)
// ---------------------------------------------------------------------------

export type PreviewAuth =
  | { ok: true; row: PortExposureRow }
  | { ok: false; status: 401 | 403 | 404 | 410 };

function constantTimeEquals(a: string, b: string): boolean {
  const ab = Buffer.from(a);
  const bb = Buffer.from(b);
  if (ab.length !== bb.length) return false;
  return timingSafeEqual(ab, bb);
}

/**
 * Resolve + authorize a preview request. `now` is injectable for tests.
 *   404 unknown slug · 410 expired · (then) share-token OR session owner/admin.
 */
export async function authorizePreview(
  opts: {
    slug: string;
    token: string | null;
    headers: Headers;
    store: PortExposureStore;
    getSession: GetSession;
    now?: Date;
  },
): Promise<PreviewAuth> {
  const row = await opts.store.getBySlug(opts.slug);
  if (!row) return { ok: false, status: 404 };

  const now = opts.now ?? new Date();
  if (row.expiresAt && row.expiresAt.getTime() <= now.getTime()) {
    return { ok: false, status: 410 };
  }

  // Share-link: a valid token for a shared exposure grants access without a
  // session (still inside the IAP wall on the internal deployment).
  if (
    row.visibility === "shared" &&
    row.shareToken &&
    opts.token &&
    constantTimeEquals(opts.token, row.shareToken)
  ) {
    return { ok: true, row };
  }

  // Otherwise require an authenticated owner or admin.
  const session = await opts.getSession(opts.headers);
  if (!session) return { ok: false, status: 401 };
  const isOwner = session.user.id === row.ownerUserId;
  const isAdmin = (session.user.role ?? "user") === "admin";
  if (!isOwner && !isAdmin) return { ok: false, status: 403 };
  return { ok: true, row };
}

// ---------------------------------------------------------------------------
// PortRelay-backed Duplex ("socket") for http.request
// ---------------------------------------------------------------------------

function openFrame(sessionId: string, port: number): RelayPortRequest {
  return create(RelayPortRequestSchema, {
    frame: { case: "open", value: { sessionId, port } },
  });
}
function dataFrame(bytes: Uint8Array): RelayPortRequest {
  return create(RelayPortRequestSchema, { frame: { case: "data", value: bytes } });
}
function closeFrame(): RelayPortRequest {
  return create(RelayPortRequestSchema, { frame: { case: "close", value: {} } });
}

/**
 * A Duplex that *is* the connection to the guest port: writes become `data`
 * frames on the relay's inbound stream; the relay's outbound `data` frames are
 * pushed onto the readable side. Handed to `http.request({ createConnection })`
 * so node drives HTTP/1.1 framing over the opaque tunnel.
 */
export function tunnelSocket(
  relay: PortRelayClient,
  sessionId: string,
  port: number,
  signal: AbortSignal,
): Duplex {
  const outbound = pushableQueue<RelayPortRequest>(256);
  outbound.push(openFrame(sessionId, port));
  const responses = relay.relay(outbound, { signal });

  let resumeRead: (() => void) | null = null;

  const duplex = new Duplex({
    write(chunk, _enc, cb) {
      outbound.push(dataFrame(new Uint8Array(chunk)));
      cb();
    },
    final(cb) {
      outbound.push(closeFrame());
      outbound.end();
      cb();
    },
    read() {
      if (resumeRead) {
        const r = resumeRead;
        resumeRead = null;
        r();
      }
    },
    destroy(err, cb) {
      outbound.end();
      cb(err);
    },
  });

  void (async () => {
    try {
      for await (const msg of responses) {
        if (msg.frame.case === "data") {
          if (!duplex.push(Buffer.from(msg.frame.value))) {
            // Backpressure: wait for the next _read() before pulling more.
            await new Promise<void>((res) => {
              resumeRead = res;
            });
          }
        } else if (msg.frame.case === "close") {
          break;
        }
      }
      duplex.push(null); // EOF
    } catch (err) {
      duplex.destroy(err as Error);
    }
  })();

  return duplex;
}

// ---------------------------------------------------------------------------
// HTTP proxy
// ---------------------------------------------------------------------------

// `duplex: "half"` is required by WHATWG fetch for a streaming request body;
// it's real at runtime (Bun + Node 18+) but missing from the DOM RequestInit
// lib type, so we widen the type rather than cast a value.
type FetchInit = RequestInit & { duplex?: "half" };

/**
 * Proxy one HTTP request to the guest port over the relay tunnel.
 *
 * Bun's `http.request` ignores a custom `createConnection` — it dials the host
 * for real (confirmed: it routes through `fetch` internally) — so we can't hand
 * it a tunnel-backed socket. Instead we stand up a one-shot loopback listener
 * that raw-pipes a real local socket to the relay tunnel, then `fetch()` that
 * local port. This keeps web types end to end (no node↔web stream casts) and
 * uses only Bun-supported APIs; the extra localhost hop is within the v1
 * latency envelope (ADR 0064 "Latency").
 */
function proxyHttp(
  relay: PortRelayClient,
  sessionId: string,
  port: number,
  c: Context,
): Promise<Response> {
  const signal = c.req.raw.signal;

  return new Promise<Response>((resolve) => {
    const server = net.createServer((sock) => {
      // Exactly one connection per request — stop accepting immediately.
      server.close();
      const tunnel = tunnelSocket(relay, sessionId, port, signal);
      sock.pipe(tunnel);
      tunnel.pipe(sock);
      const tearDown = () => {
        sock.destroy();
        tunnel.destroy();
      };
      sock.on("error", tearDown);
      tunnel.on("error", tearDown);
    });

    server.on("error", () =>
      resolve(new Response("preview proxy error", { status: 502 })),
    );

    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      const localPort = addr && typeof addr === "object" ? addr.port : 0;
      const url = new URL(c.req.url);
      // Rewrite Host so guest apps that self-reference localhost keep working.
      const headers = new Headers(c.req.raw.headers);
      headers.set("host", `localhost:${port}`);

      const init: FetchInit = {
        method: c.req.method,
        headers,
        body: c.req.raw.body,
        redirect: "manual",
        signal,
      };
      if (c.req.raw.body) init.duplex = "half";

      fetch(`http://127.0.0.1:${localPort}${url.pathname}${url.search}`, init)
        .then((upstream) =>
          resolve(
            new Response(upstream.body, {
              status: upstream.status,
              headers: upstream.headers,
            }),
          ),
        )
        .catch(() => {
          server.close();
          resolve(new Response("preview upstream error", { status: 502 }));
        });
    });
  });
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/**
 * Hono middleware: when the request Host is a preview host, resolve + authorize
 * the slug and proxy to the guest port; otherwise pass through to the app.
 * Mount FIRST (before the normal routes) in index.ts.
 */
export function makePreviewProxyMiddleware(deps?: PreviewProxyDeps): MiddlewareHandler {
  const baseDomain = deps?.previewBaseDomain ?? config.previewBaseDomain;
  const relay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  // Lazy store so importing this module doesn't call getDb() at load.
  let store = deps?.store;
  const getStore = (): PortExposureStore => (store ??= makePortExposureStore());
  const resolveSession: GetSession =
    deps?.getSession ??
    (async (headers) => {
      const { auth } = await import("../auth/better-auth.ts");
      return auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]);
    });

  return async (c, next) => {
    const slug = previewSlugFromHost(c.req.header("host"), baseDomain);
    if (!slug) return next();

    const token = new URL(c.req.url).searchParams.get("token");
    const authz = await authorizePreview({
      slug,
      token,
      headers: c.req.raw.headers,
      store: getStore(),
      getSession: resolveSession,
    });
    if (!authz.ok) {
      const msg =
        authz.status === 401
          ? "unauthenticated"
          : authz.status === 403
            ? "forbidden"
            : authz.status === 410
              ? "preview expired"
              : "not found";
      return c.text(msg, authz.status);
    }

    return proxyHttp(relay, authz.row.sessionId, authz.row.port, c);
  };
}
