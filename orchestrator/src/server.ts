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
 * RUNTIME OVERRIDE (ADR 0039 said Node; user chose Bun 2026-06-11):
 *   Bun implements node:http fully, so node:http createServer + connectNodeAdapter
 *   + @hono/node-server's getRequestListener all run unchanged under Bun.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { Hono } from "hono";
import { getRequestListener } from "@hono/node-server";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";

export type RouteRegistrar = (router: ConnectRouter) => void;

/**
 * Build and return a node:http Server that:
 *   - routes /rpc/* to the Connect adapter (empty routes by default; later
 *     tasks register real services by passing a `routes` function)
 *   - routes everything else to the Hono app via getRequestListener
 *
 * Exported so tests can call buildServer(...) on an ephemeral port.
 */
export function buildServer(app: Hono, routes: RouteRegistrar = () => {}) {
  // requestPathPrefix must match the "/rpc/" seam — handlers register at
  // prefix+requestPath, so a missing prefix causes every real RPC to 404.
  const connectHandler = connectNodeAdapter({ routes, requestPathPrefix: "/rpc" });

  // getRequestListener is pure node:http code (no Bun-specific APIs).
  // It handles SSE streaming, multiple Set-Cookie headers, and client-disconnect
  // abort correctly — unlike the hand-rolled fetch bridge it replaces.
  const honoListener = getRequestListener(app.fetch);

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const url = req.url ?? "/";

    if (url.startsWith("/rpc/") || url === "/rpc") {
      // Connect adapter takes over: handles Connect/gRPC/gRPC-Web protocols.
      connectHandler(req, res);
      return;
    }

    // Everything else: delegate to Hono via getRequestListener.
    honoListener(req, res);
  });

  return server;
}
