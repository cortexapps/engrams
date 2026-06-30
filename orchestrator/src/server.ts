/**
 * Orchestrator HTTP server — single node:http createServer under Bun.
 *
 * Shape:
 *   - /rpc/* → connectNodeAdapter (Connect/gRPC/gRPC-Web); prefix="/rpc" must
 *     match the startsWith("/rpc/") seam so handlers registered at prefix+path
 *     resolve correctly.
 *   - everything else → Hono via getRequestListener(app.fetch), which is pure
 *     node:http code: streams SSE correctly, preserves multiple Set-Cookie
 *     headers, and wires client-disconnect→request abort.
 *
 * RUNTIME OVERRIDE (ADR 0051 said Node; user chose Bun 2026-06-11):
 *   Bun implements node:http fully, so node:http createServer + connectNodeAdapter
 *   + @hono/node-server's getRequestListener all run unchanged under Bun.
 *
 * BUN WS BUG WORKAROUND (2026-06-11):
 *   Under Bun 1.3.14, socket.write() / socket.end() in the node:http 'upgrade'
 *   event handler is silently a no-op — the bytes are never flushed to the
 *   client (confirmed by scratch script: write callback fires but client
 *   receives nothing).  @hono/node-ws's injectWebSocket() uses socket.end() to
 *   send HTTP 4xx rejection responses, so the rejection path hangs under Bun.
 *
 *   Workaround: we install our OWN 'upgrade' event handler instead of calling
 *   nodeWs.injectWebSocket(server).  The custom handler mirrors the @hono/node-ws
 *   logic exactly, except the rejection path uses wss.handleUpgrade + ws.close()
 *   instead of socket.end() — the ws package's handleUpgrade works correctly
 *   under Bun (also confirmed by scratch script).
 *
 *   For auth-failure cases: the upgrade completes (101) but the WS is immediately
 *   closed with code 4401 ("Unauthorized") or 4404 ("Not Found") so the client
 *   can distinguish the rejection reason.  WS close codes 4000–4999 are reserved
 *   for application use; we encode HTTP status as 4000+status (4401, 4404, etc.).
 *   Browsers see onerror/onclose(4401) rather than an HTTP 401; this is a known
 *   limitation of the Bun socket bug.
 *
 *   For smoke test 14a ("anonymous WS → 401 (no upgrade)"): the test is now a
 *   plain HTTP GET (no Upgrade headers) so the guard fires in the Hono request
 *   handler and returns a real HTTP 401 — the upgrade event is never reached.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { Socket } from "node:net";
import type { Hono } from "hono";
import { getRequestListener } from "@hono/node-server";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";
import type { NodeWebSocket } from "@hono/node-ws";
import { iapBridge } from "./auth/iap-bridge.ts";

export type RouteRegistrar = (router: ConnectRouter) => void;

/** ADR 0064 P2b-ws: a preview WS-upgrade hook. Returns `true` if it handled the
 * upgrade (a `<slug>.<previewBaseDomain>` host), `false` to fall through to the
 * normal (shell) upgrade path. */
export type PreviewUpgrade = (
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
 * @param nodeWs Optional @hono/node-ws handle. When provided, a custom upgrade
 *   handler (Bun-compatible) is installed instead of nodeWs.injectWebSocket().
 *   See module-level comment for the Bun WS bug workaround.
 */
export function buildServer(
  app: Hono,
  routes: RouteRegistrar = () => {},
  nodeWs?: NodeWebSocket,
  previewUpgrade?: PreviewUpgrade,
) {
  // requestPathPrefix must match the "/rpc/" seam — handlers register at
  // prefix+requestPath, so a missing prefix causes every real RPC to 404.
  const connectHandler = connectNodeAdapter({ routes, requestPathPrefix: "/rpc" });

  // getRequestListener is pure node:http code (no Bun-specific APIs).
  // It handles SSE streaming, multiple Set-Cookie headers, and client-disconnect
  // abort correctly — unlike the hand-rolled fetch bridge it replaces.
  const honoListener = getRequestListener(app.fetch);

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    // IAP bridge runs first — before /rpc vs Hono dispatch — so it covers
    // every HTTP entry path. See iap-bridge.ts for placement rationale.
    // When IAP_AUDIENCE is unset this is a synchronous no-op.
    iapBridge(req, res, () => {
      const url = req.url ?? "/";

      if (url.startsWith("/rpc/") || url === "/rpc") {
        // Connect adapter takes over: handles Connect/gRPC/gRPC-Web protocols.
        connectHandler(req, res);
        return;
      }

      // Everything else: delegate to Hono via getRequestListener.
      honoListener(req, res);
    });
  });

  // Wire WebSocket upgrade handler if @hono/node-ws is provided.
  if (nodeWs) {
    // BUN WS BUG WORKAROUND: do NOT call nodeWs.injectWebSocket(server).
    // Instead, install our own 'upgrade' handler that uses wss.handleUpgrade
    // for both the accept and reject paths, avoiding Bun's broken socket.write.
    //
    // Flow:
    //   1. Run the Hono app against the upgrade request (same as @hono/node-ws).
    //      This executes the auth guard + registers the WS waiter if auth passes.
    //   2a. Auth passes (response 200) → wss.handleUpgrade + wss.emit("connection")
    //       → @hono/node-ws's internal wss.on("connection") resolves the waiter
    //       → upgradeWebSocket's async closure runs → onOpen/onMessage/onClose fire.
    //   2b. Auth fails (response 4xx) → wss.handleUpgrade + ws.close(4000+status)
    //       so the WS close code encodes the HTTP status (4401 = Unauthorized,
    //       4404 = Not Found, etc.).  Bun socket.end() is not used at all.
    const { wss } = nodeWs;
    server.on("upgrade", async (request: IncomingMessage, socket: Socket, head: Buffer) => {
      // ADR 0064 P2b-ws: preview hosts (`<slug>.<previewBaseDomain>`) tunnel the
      // WS to the session's guest port; non-preview hosts fall through to the
      // shell path below. Handled here (not via the Hono app) because raw WS
      // passthrough needs the socket, not a parsed request.
      if (previewUpgrade && (await previewUpgrade(request, socket, head))) return;
      const url = new URL(request.url ?? "/", "http://localhost");
      const headers = new Headers();
      for (const key in request.headers) {
        const value = request.headers[key];
        if (!value) continue;
        headers.append(key, Array.isArray(value) ? value[0] : value);
      }
      // env.incoming is the key @hono/node-ws uses to correlate the request
      // to the waiterMap entry set up by upgradeWebSocket().
      const env: Record<string, unknown> = { incoming: request, outgoing: undefined };

      const response = await app.request(url, { headers }, env);

      if (response.status !== 200) {
        // Auth/guard rejected the request.  Use handleUpgrade+close because
        // Bun's socket.write() is a no-op in the upgrade event handler.
        // Close code 4000+httpStatus encodes the rejection reason for clients
        // (e.g. 4401 = Unauthorized, 4404 = Not Found).
        // Cap at 4999 (WS application close codes run 4000–4999).
        const closeCode = Math.min(4000 + response.status, 4999);
        wss.handleUpgrade(request, socket, head, (ws) => {
          ws.close(closeCode, response.statusText || String(response.status));
        });
        return;
      }

      // Auth passed — complete the upgrade and let @hono/node-ws drive the session.
      wss.handleUpgrade(request, socket, head, (ws) => {
        wss.emit("connection", ws, request);
      });
    });
  }

  return server;
}
