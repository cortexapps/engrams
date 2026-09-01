/**
 * Orchestrator HTTP server — single node:http createServer under Bun.
 *
 * Shape:
 *   - /rpc/* → connectNodeAdapter (Connect/gRPC/gRPC-Web); prefix="/rpc" must
 *     match the startsWith("/rpc/") seam so handlers registered at prefix+path
 *     resolve correctly. NOT for a preview host — see the dispatch below.
 *   - everything else → Hono via getRequestListener(app.fetch), which is pure
 *     node:http code: streams SSE correctly, preserves multiple Set-Cookie
 *     headers, and wires client-disconnect→request abort.
 *   - WebSocket upgrades → the UpgradeHook list, tried in order. EVERY WS
 *     surface is an accept-first raw handler (routes/ws-util.ts documents the
 *     Bun invariant: the handshake must complete inside the request's own
 *     event-loop turn, before any awaited I/O). The old @hono/node-ws path —
 *     and the Bun socket.write workaround it needed — is retired; an upgrade
 *     no hook claims is destroyed.
 *
 * RUNTIME OVERRIDE (ADR 0051 said Node; user chose Bun 2026-06-11):
 *   Bun implements node:http fully, so node:http createServer + connectNodeAdapter
 *   + @hono/node-server's getRequestListener all run unchanged under Bun.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { Socket } from "node:net";
import type { Hono } from "hono";
import { getRequestListener } from "@hono/node-server";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";
import { iapBridge } from "./auth/iap-bridge.ts";
import { applyCors, wsOriginAllowed } from "./auth/cors.ts";
import { isUnderPreviewDomain } from "./apps/hostname.ts";
import { config } from "./config.ts";
import { log } from "./log.ts";

export type RouteRegistrar = (router: ConnectRouter) => void;

/** A raw WS-upgrade hook, tried in order; an upgrade no hook claims is
 * destroyed. Returns `true` if it handled the upgrade, `false` to fall
 * through to the next hook. Raw (not via the Hono app) because every WS
 * surface needs the accept-first handshake (routes/ws-util.ts). Today's
 * hooks: the preview proxy (ADR 0064 P2b-ws, Host-keyed), the IDE proxy
 * (ADR 0085), spec sync (ADR 0117), the shell (ADR 0051) and VNC
 * (ADR 0065/0066), all path-keyed. */
export type UpgradeHook = (
  req: IncomingMessage,
  socket: Socket,
  head: Buffer,
) => Promise<boolean>;

/**
 * Build and return a node:http Server that:
 *   - routes /rpc/* to the Connect adapter (empty routes by default; later
 *     tasks register real services by passing a `routes` function)
 *   - routes everything else to the Hono app via getRequestListener
 *
 * Exported so tests can call buildServer(...) on an ephemeral port.
 *
 */
export function buildServer(
  app: Hono,
  routes: RouteRegistrar = () => {},
  upgradeHooks: UpgradeHook[] = [],
  /** Preview base domain, for the termination check below. Defaults to config;
   *  a test that passes "" opts out, since no host is under an empty domain. */
  previewBaseDomain: string = config.previewBaseDomain,
) {
  // requestPathPrefix must match the "/rpc/" seam — handlers register at
  // prefix+requestPath, so a missing prefix causes every real RPC to 404.
  const connectHandler = connectNodeAdapter({ routes, requestPathPrefix: "/rpc" });

  // getRequestListener is pure node:http code (no Bun-specific APIs).
  // It handles SSE streaming, multiple Set-Cookie headers, and client-disconnect
  // abort correctly — unlike the hand-rolled fetch bridge it replaces.
  const honoListener = getRequestListener(app.fetch);

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    // CORS gate runs before EVERYTHING — a preflight OPTIONS carries no
    // cookie and no IAP assertion by spec, so the fail-closed bridge below
    // would 401 it and no cross-origin call (split-host SPA → api host)
    // could ever start. See auth/cors.ts.
    //
    // Preview hosts are exempt: ADR 0118 forwards a preview preflight to the
    // guest app, whose own CORS policy answers it (see preview-proxy.ts).
    // Gating here would swallow it and break every cross-app call a session
    // app makes.
    if (
      !isUnderPreviewDomain(req.headers.host, previewBaseDomain) &&
      applyCors(req, res)
    ) {
      return;
    }

    // IAP bridge runs next — before /rpc vs Hono dispatch — so it covers
    // every HTTP entry path. See iap-bridge.ts for placement rationale.
    // When IAP_AUDIENCES is empty this is a synchronous no-op.
    iapBridge(req, res, () => {
      const url = req.url ?? "/";

      // INVARIANT — preview hosts terminate (ADR 0118). This dispatch is the
      // one place that can break it, because it is PATH-keyed and runs BEFORE
      // Hono, where the preview wall lives. Without the host check a request to
      // `<app>.preview.<domain>/rpc/...` is answered by the ORCHESTRATOR'S OWN
      // Connect services — and the visitor's cookie is scoped to the parent
      // domain, so the browser attaches it and the call succeeds as that user.
      //
      // That is a real escalation, not a routing curiosity: a session app's
      // page is written by the agent, and this seam let it drive the control
      // plane as whoever opened it. It also walked straight around the
      // credential-stripping boundary in the preview proxy, whose entire job is
      // to keep the visitor's session token away from guest code. Found in
      // prod: a nested engrams' SPA calls same-origin `/rpc`, and the outer
      // orchestrator answered it with the visitor's real task list.
      //
      // A preview host therefore goes to Hono, where the wall answers it and
      // the proxy forwards it to the guest — which is also what the nested app
      // wanted: its own `/rpc`, served by its own orchestrator.
      const isPreviewHost = isUnderPreviewDomain(req.headers.host, previewBaseDomain);
      if (!isPreviewHost && (url.startsWith("/rpc/") || url === "/rpc")) {
        // Connect adapter takes over: handles Connect/gRPC/gRPC-Web protocols.
        connectHandler(req, res);
        return;
      }

      // Everything else: delegate to Hono via getRequestListener.
      honoListener(req, res);
    });
  });

  // WebSocket upgrades: dispatch to the accept-first hooks.
  server.on("upgrade", async (request: IncomingMessage, socket: Socket, head: Buffer) => {
    // EVERYTHING below runs inside this try. An `upgrade` listener is async,
    // so anything it throws becomes an unhandled rejection — and under Bun
    // that exits the process. One client's failed handshake then takes the
    // whole orchestrator down, which is exactly what happened in prod on
    // 2026-08-20: a preview WebSocket hit an error path inside `ws`
    // (abortHandshake with a code http.STATUS_CODES has no entry for), the
    // rejection escaped, and both pods crash-looped 11 times.
    //
    // An upgrade concerns ONE connection. The blast radius has to be that
    // connection, so the catch destroys the socket and nothing else.
    try {
      await handleUpgradeRequest(request, socket, head);
    } catch (err) {
      log.error(
        { component: "ws", err, url: request.url, host: request.headers.host },
        "websocket upgrade failed; destroying the socket",
      );
      // Best-effort: the socket may already be gone, which is frequently the
      // reason we are here at all.
      try {
        socket.destroy();
      } catch {
        /* already destroyed */
      }
    }
  });

  async function handleUpgradeRequest(
    request: IncomingMessage,
    socket: Socket,
    head: Buffer,
  ): Promise<void> {
    // Refuse a malformed handshake before `ws` ever sees it.
    //
    // This is what took prod down on 2026-08-20. An upgrade with no valid
    // `Sec-WebSocket-Key` drives Bun's BUILTIN `ws` into its abort path —
    // note the crash frames read `ws:671` with no file, so the npm package in
    // node_modules is not what runs — and that path mishandles its own
    // arguments. Reproduced locally on Bun 1.3.14, where it answers
    // `HTTP/1.1 400 [object Object]`; on the 1.4.0 the pods run it throws
    // `TypeError: undefined is not an object (evaluating 'message')` instead,
    // which is how one bad handshake killed the process.
    //
    // Dropping the socket is the only available answer, not a shortcut:
    // `socket.end(...)` in an upgrade listener is a NO-OP under Bun (measured
    // — the client receives nothing), and completing the handshake to send a
    // close frame needs the very key that is missing. A client that omits it
    // is not a conforming WebSocket client, so there is nobody to explain
    // ourselves to. Preview hostnames are internet-reachable (ADR 0118 moved
    // the wall into this process), so this arrives as background noise.
    const wsKey = request.headers["sec-websocket-key"];
    const wsVersion = request.headers["sec-websocket-version"];
    if (typeof wsKey !== "string" || wsKey.length === 0 || wsVersion !== "13") {
      log.warn(
        {
          component: "ws",
          host: request.headers.host,
          url: request.url,
          hasKey: typeof wsKey === "string" && wsKey.length > 0,
          version: wsVersion,
        },
        "refusing a malformed websocket upgrade",
      );
      socket.destroy();
      return;
    }

    // Cross-site WS handshakes are refused up front. Browsers attach the
    // session cookie to a WS opened by ANY page (WebSockets are outside
    // CORS), and with the cookie widened to the parent domain for the
    // split-host layout, a guest-authored preview page could otherwise
    // open an authenticated socket to the api host. No Origin header
    // (CLI / non-browser clients) passes — their auth is the route guard.
    //
    // Preview hosts are exempt, mirroring the HTTP CORS gate: ADR 0118
    // terminates preview upgrades at the proxy hook below, whose wall +
    // cookie-strip is the policy there — and sibling apps of one session
    // legitimately open sockets to each other (dev-server proxies, HMR),
    // which this gate would otherwise kill before the hook ever ran.
    // This gate protects the ORCHESTRATOR'S OWN routes.
    const isPreviewUpgrade = isUnderPreviewDomain(request.headers.host, previewBaseDomain);
    if (!isPreviewUpgrade && !wsOriginAllowed(request)) {
      log.warn(
        {
          component: "ws",
          host: request.headers.host,
          url: request.url,
          origin: request.headers.origin,
        },
        "refusing a cross-origin websocket upgrade",
      );
      socket.destroy();
      return;
    }

    // Accept-first raw handlers (preview by Host; IDE, spec-sync, shell and
    // VNC by path — see UpgradeHook), tried in order.
    for (const hook of upgradeHooks) {
      if (await hook(request, socket, head)) return;
    }

    // No hook claimed it: not a WebSocket surface this server offers.
    log.warn(
      { component: "ws", host: request.headers.host, url: request.url },
      "unmatched websocket upgrade; destroying the socket",
    );
    socket.destroy();
  }

  return server;
}
