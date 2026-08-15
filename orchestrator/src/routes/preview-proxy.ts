/**
 * Session-app preview reverse-proxy (ADR 0064 P2b, model per ADR 0118) — HTTP.
 *
 * A request to `<host label>.<previewBaseDomain>` is proxied to the guest's
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
 * Auth: the host label resolves to a `session_app` row; access requires the
 * authenticated principal to own the session or be an admin. IAP still fronts
 * everything on a deployment that runs it, so this is the second gate. ADR 0118
 * P2 moves the wall here entirely (and adds org visibility, the login redirect,
 * the unauthenticated `OPTIONS` path, and sibling-origin enforcement).
 *
 * WebSocket upgrades are handled separately (P2b-ws) — Bun's node:http `upgrade`
 * handler can't write to the raw socket (see server.ts), so raw WS passthrough
 * needs its own approach. Plain HTTP (page loads, assets, API, SSE) works here.
 *
 * Deps are injectable for tests; real singletons are used when omitted.
 */

import net from "node:net";
import { Duplex } from "node:stream";
import type { Context, MiddlewareHandler } from "hono";
import { create } from "@bufbuild/protobuf";
import { ConnectError, Code } from "@connectrpc/connect";
import {
  RelayPortRequestSchema,
  type RelayPortRequest,
  type RelayPortResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import { portRelay as defaultPortRelay } from "../control-plane/client.ts";
import { config } from "../config.ts";
import { isValidHostLabel } from "../apps/hostname.ts";
import {
  makeSessionAppStore,
  type SessionAppRow,
  type SessionAppStore,
} from "../db/session-apps.ts";
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
  store?: SessionAppStore;
  portRelay?: PortRelayClient;
  getSession?: GetSession;
  previewBaseDomain?: string;
}

// ---------------------------------------------------------------------------
// Host → slug parsing (pure)
// ---------------------------------------------------------------------------

/**
 * If `hostHeader` is `<label>.<baseDomain>` for a single valid label, return the
 * label; otherwise null (apex, multi-level, or foreign host). `baseDomain`
 * carries its port in dev (`lvh.me:8787`) and none in prod (behind the :443 LB)
 * — we compare verbatim.
 */
export function previewHostLabel(
  hostHeader: string | undefined,
  baseDomain: string,
): string | null {
  if (!hostHeader) return null;
  const host = hostHeader.toLowerCase();
  const suffix = "." + baseDomain.toLowerCase();
  if (!host.endsWith(suffix)) return null;
  const label = host.slice(0, -suffix.length);
  // Must be a single DNS label (no nested subdomain) and label-shaped.
  if (label.includes(".") || !isValidHostLabel(label)) return null;
  return label;
}

// ---------------------------------------------------------------------------
// Authorization (pure-ish: deps injected)
// ---------------------------------------------------------------------------

export type PreviewAuth =
  | { ok: true; row: SessionAppRow }
  | { ok: false; status: 401 | 403 | 404 };

/**
 * Resolve + authorize a preview request.
 *   404 unknown host label · 401 no session · 403 not owner/admin.
 *
 * ADR 0118 retired ADR 0064's unauthenticated `shareToken` capability: the
 * requirement is that nobody reaches an app without passing the login wall, and
 * a token in a URL is exactly a way around it. It also retired `expiresAt`,
 * which was enforced here but never written by anything.
 *
 * The `visibility = "org"` widening (any authenticated principal, so a teammate
 * can open your preview) lands with the rest of the wall move in P2. Until then
 * this stays at owner-or-admin, which is the stricter of the two.
 */
export async function authorizePreview(
  opts: {
    hostLabel: string;
    headers: Headers;
    store: SessionAppStore;
    getSession: GetSession;
  },
): Promise<PreviewAuth> {
  const row = await opts.store.getByHostLabel(opts.hostLabel);
  if (!row) return { ok: false, status: 404 };

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
 *
 * `targetPath` overrides the pathname+search sent to the guest (defaults to
 * the incoming request's own `url.pathname + url.search`). ADR 0085's IDE
 * proxy (`routes/ide.ts`) uses this to strip the `/api/v1/sessions/:id/ide`
 * route prefix before forwarding to code-server, which expects root-relative
 * paths — everything else about the mechanics (loopback hop, header
 * forwarding, content-encoding/length strip, streaming body) is unchanged and
 * shared verbatim.
 */
export function proxyHttp(
  relay: PortRelayClient,
  sessionId: string,
  port: number,
  c: Context,
  targetPath?: string,
): Promise<Response> {
  const signal = c.req.raw.signal;

  return new Promise<Response>((resolve) => {
    // Captured from the relay tunnel so the terminal error path can distinguish
    // the coordinator's `resource_exhausted` (session at its preview-connection
    // cap, ADR 0066) → 503, from a generic upstream failure → 502.
    let tunnelError: unknown;
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
      tunnel.on("error", (err) => {
        tunnelError = err;
        tearDown();
      });
    });

    server.on("error", () => resolve(previewErrorResponse(tunnelError)));

    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      const localPort = addr && typeof addr === "object" ? addr.port : 0;
      const path = targetPath ?? (() => {
        const url = new URL(c.req.url);
        return url.pathname + url.search;
      })();
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

      fetch(`http://127.0.0.1:${localPort}${path}`, init)
        .then((upstream) => {
          // WHATWG fetch transparently DECOMPRESSES a Content-Encoding'd
          // upstream body but leaves the original entity headers in place, so
          // forwarding `upstream.headers` verbatim sends the decompressed
          // bytes labeled with the guest's `content-encoding: gzip` (and the
          // compressed-size `content-length`) — the browser then fails to
          // gunzip plain bytes (net::ERR_CONTENT_DECODING_FAILED). Strip both
          // so the forwarded headers describe the body we actually have.
          const headers = new Headers(upstream.headers);
          headers.delete("content-encoding");
          headers.delete("content-length");
          resolve(
            new Response(upstream.body, {
              status: upstream.status,
              headers,
            }),
          );
        })
        .catch(() => {
          server.close();
          resolve(previewErrorResponse(tunnelError));
        });
    });
  });
}

/**
 * Map a relay-tunnel failure to an HTTP status. The coordinator returns
 * `resource_exhausted` (ADR 0066) when a session is at its concurrent
 * preview-connection cap — surface that as a **503** (retryable) rather than a
 * generic **502**, so the browser/UI can distinguish "too many open previews"
 * from "the guest server is down."
 */
function previewErrorResponse(err: unknown): Response {
  if (err instanceof ConnectError && err.code === Code.ResourceExhausted) {
    return new Response("too many concurrent preview connections", { status: 503 });
  }
  return new Response("preview upstream error", { status: 502 });
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
  const getStore = (): SessionAppStore => (store ??= makeSessionAppStore());
  const resolveSession: GetSession =
    deps?.getSession ??
    (async (headers) => {
      const { getSessionFromHeaders } = await import("../auth/session.ts");
      return getSessionFromHeaders(headers);
    });

  return async (c, next) => {
    const slug = previewHostLabel(c.req.header("host"), baseDomain);
    if (!slug) return next();

    const authz = await authorizePreview({
      hostLabel: slug,
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
            : "not found";
      return c.text(msg, authz.status);
    }

    return proxyHttp(relay, authz.row.sessionId, authz.row.port, c);
  };
}
