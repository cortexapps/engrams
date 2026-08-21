/**
 * In-guest IDE proxy (ADR 0085 P4) — session-scoped HTTP + WebSocket.
 *
 * ALL /api/v1/sessions/:id/ide/*  (+ a redirect for the bare `/ide`)
 *
 * code-server (VS Code in the guest) serves plain HTTP assets AND speaks its
 * own WebSocket protocol on the same loopback port. This route fills the
 * missing proxy quadrant (ADR 0085 "Context"): session-path + better-auth
 * guard × full HTTP+WS passthrough.
 *
 *   - Auth: the same better-auth + CASL gate as /shell and /vnc, re-run on
 *     every request (owner-only surface, no share tokens).
 *   - Lifecycle: `SessionService.EnsureIde` before every proxy leg — it
 *     fast-paths when code-server is already up and auto-resumes an idle
 *     session (desired: opening the IDE tab wakes the session).
 *   - HTTP: the route prefix is stripped so code-server sees root-relative
 *     paths (its client uses relative asset paths behind path-rewriting
 *     proxies), then the request rides `preview-proxy.ts`'s `proxyHttp`
 *     (loopback net.Server + fetch over the PortRelay tunnel, including the
 *     content-encoding/content-length strip, #616).
 *   - WS: code-server performs its own guest-side handshake, so upgrades use
 *     `preview-ws.ts`'s `bridgeClientToGuest` (terminate the client WS, dial a
 *     native WebSocket through the loopback-bridged tunnel), NOT vnc.ts's raw
 *     pump. Wired as an upgrade hook in server.ts's chain, keyed on the
 *     session path prefix (the preview hook is keyed on Host and runs first).
 */

import { Hono } from "hono";
import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import { WebSocketServer } from "ws";

import { sessions as defaultSessions, portRelay as defaultPortRelay } from "../control-plane/client.ts";
import { proxyHttp, type PortRelayClient } from "./preview-proxy.ts";
import { bridgeClientToGuest } from "./preview-ws.ts";
import { makeGuard, makeHeaderGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Minimal structural interface of the SessionService client we depend on —
 * the real Connect client satisfies it; tests hand in a plain fake. */
export interface IdeSessionsClient {
  ensureIde(
    req: { sessionId: string },
    options?: { signal?: AbortSignal },
  ): Promise<{ port: number }>;
}

export interface IdeDeps {
  /** Control-plane SessionService client (for EnsureIde). */
  sessions?: IdeSessionsClient;
  /** PortRelay client for the HTTP/WS tunnel to the guest. */
  portRelay?: PortRelayClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

/** Match an IDE path: `/api/v1/sessions/<id>/ide` or `/api/v1/sessions/<id>/ide/<rest>`.
 * Group 1 = the raw session id segment, group 2 = the guest-relative path
 * (leading `/`, absent for the bare `/ide`). */
const IDE_PATH_RE = /^\/api\/v1\/sessions\/([^/]+)\/ide(\/.*)?$/;

/** Split an incoming IDE URL into `{ sessionId, guestPath }`, where
 * `guestPath` is the prefix-stripped path+query code-server should see
 * (`/`, `/static/...?v=1`, ...). Returns null for non-IDE paths. */
export function parseIdePath(pathname: string, search: string): { sessionId: string; guestPath: string } | null {
  const m = IDE_PATH_RE.exec(pathname);
  if (!m) return null;
  return {
    sessionId: decodeURIComponent(m[1]!),
    guestPath: (m[2] ?? "/") + search,
  };
}

// ---------------------------------------------------------------------------
// HTTP route factory
// ---------------------------------------------------------------------------

export function makeIdeRoute(deps?: IdeDeps): { app: Hono } {
  const app = new Hono();
  const sessions: IdeSessionsClient = deps?.sessions ?? defaultSessions;
  const relay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  // Bare `/ide` → `/ide/` so code-server's relative asset paths resolve
  // against the prefixed directory. 307 preserves method + body; the query
  // string is carried over. Unguarded: the redirect is static (same for every
  // session id) so it reveals nothing — the target re-runs the guard.
  app.all("/api/v1/sessions/:id/ide", (c) => {
    const url = new URL(c.req.url);
    return c.redirect(`${url.pathname}/${url.search}`, 307);
  });

  app.all("/api/v1/sessions/:id/ide/*", async (c) => {
    // 1. Auth + ownership — plain 401/404. Same bar as the shell/vnc tabs.
    await guardFn(c, "shell");

    const url = new URL(c.req.url);
    const parsed = parseIdePath(url.pathname, url.search);
    if (!parsed) return c.text("not found", 404); // unreachable given the route pattern

    // 2. Ensure code-server is up in the guest (idempotent; auto-resumes an
    //    idle session) and learn its loopback port.
    let port: number;
    try {
      ({ port } = await sessions.ensureIde(
        { sessionId: parsed.sessionId },
        { signal: c.req.raw.signal },
      ));
    } catch (err) {
      console.warn({ err, sessionId: parsed.sessionId }, "ide ensureIde failed");
      return c.text("ide start failed", 502);
    }

    // 3. Proxy over the PortRelay tunnel with the route prefix stripped so
    //    code-server sees root-relative paths. proxyHttp streams the response
    //    body and strips content-encoding/content-length (#616).
    return proxyHttp(relay, parsed.sessionId, port, c, parsed.guestPath);
  });

  return { app };
}

// ---------------------------------------------------------------------------
// WS upgrade hook (for server.ts's dispatch chain)
// ---------------------------------------------------------------------------

/** Build the `server.on("upgrade")` hook for IDE WebSockets. Returns `true`
 * if it handled the request (an `/api/v1/sessions/:id/ide/*` path), `false`
 * to let the next hook / the shell+vnc @hono/node-ws path run. */
export function makeIdeUpgradeHandler(
  deps?: IdeDeps,
): (req: IncomingMessage, socket: Socket, head: Buffer) => Promise<boolean> {
  const sessions: IdeSessionsClient = deps?.sessions ?? defaultSessions;
  const relay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  const headerGuard = makeHeaderGuard(deps?.getSession, deps?.resolveOwner);

  // A dedicated WS server (noServer) — NOT the shell's, whose handleProtocols
  // is pinned to `tty`/`binary`. Echo the client's first requested subprotocol
  // so code-server's negotiation passes through (same as preview-ws.ts).
  const wss = new WebSocketServer({
    noServer: true,
    handleProtocols: (protocols: Set<string>) => {
      const first = protocols.values().next().value;
      return first ?? false;
    },
  });

  return async function tryIdeUpgrade(req, socket, head) {
    const url = new URL(req.url ?? "/", "http://localhost");
    const parsed = parseIdePath(url.pathname, url.search);
    if (!parsed) return false; // not an IDE path — fall through

    const headers = new Headers();
    for (const key in req.headers) {
      const v = req.headers[key];
      if (v) headers.set(key, Array.isArray(v) ? v[0]! : v);
    }

    const subprotocols = (req.headers["sec-websocket-protocol"] ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    // Complete the client upgrade BEFORE the auth guard, not after.
    //
    // Under Bun, `ws` is the runtime's builtin shim and its `completeUpgrade`
    // delegates to native `server.upgrade(req)`, which is only valid inside the
    // request's OWN event-loop turn. `headerGuard` reads Postgres, so awaiting
    // it first burned that window: `server.upgrade()` returned false and the
    // shim called `abortHandshake` with an undefined response, throwing
    // `TypeError: undefined is not an object (evaluating 'message')` out of
    // this function. That is the error that was flooding the prod logs, and
    // every IDE socket 502'd because of it.
    //
    // Authorization still happens before any bridge is built; a refused caller
    // gets the same 4000+status close it always did, just after the handshake
    // rather than instead of it.
    wss.handleUpgrade(req, socket, head, (clientWs) => {
      // Watch for a hang-up from the instant the handshake completes.
      // `bridgeClientToGuest` attaches its own close/error handlers, but only
      // after the guard and EnsureIde below — and an EventEmitter does not
      // replay a `close` that fired with no listener. EnsureIde can take
      // SECONDS when it auto-resumes an evicted session, which makes this
      // window wide: bridging a client that has already gone would leave the
      // loopback server, the relay tunnel and the guest socket standing with
      // nothing left to tear them down.
      let closedEarly = false;
      clientWs.once("close", () => {
        closedEarly = true;
      });
      const gone = () => closedEarly || clientWs.readyState !== clientWs.OPEN;

      void (async () => {
        const authz = await headerGuard(headers, parsed.sessionId, "shell");
        if (gone()) return;
        if (!authz.ok) {
          try {
            clientWs.close(4000 + authz.status, `ide ${authz.status}`);
          } catch {
            /* already closing */
          }
          return;
        }
        // EnsureIde may auto-resume an evicted session (seconds); the client is
        // already upgraded, so it waits on an open socket. Failure closes 1011.
        let port: number;
        try {
          ({ port } = await sessions.ensureIde({ sessionId: parsed.sessionId }));
        } catch (err) {
          console.warn({ err, sessionId: parsed.sessionId }, "ide ensureIde failed (ws)");
          try {
            clientWs.close(1011, "ide start failed");
          } catch {
            /* already closing */
          }
          return;
        }
        if (gone()) return;
        // Bridge with the prefix-stripped URL so code-server handshakes on the
        // path (+ query) its client actually asked for.
        const guestUrl = new URL(parsed.guestPath, "http://localhost");
        bridgeClientToGuest(clientWs, relay, parsed.sessionId, port, guestUrl, subprotocols);
      })();
    });
    return true;
  };
}
