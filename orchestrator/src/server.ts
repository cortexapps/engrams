/**
 * Orchestrator HTTP server — Hono on Bun, /rpc/* via Connect adapter.
 *
 * RUNTIME OVERRIDE (ADR 0039 said Node; user chose Bun 2026-06-11):
 *   The plan originally specified `@hono/node-server`. This server instead
 *   uses Bun's built-in HTTP server (`Bun.serve`), which is significantly
 *   faster and has first-class TypeScript support with no adapter needed.
 *
 * SHAPE CHOSEN: (a) node-compat with connectNodeAdapter + Bun.serve.
 *   Bun implements the node:http IncomingMessage / ServerResponse interfaces,
 *   but `Bun.serve` does NOT use that path — it uses its own fetch-based
 *   handler. `connectNodeAdapter` returns a node:http style listener, which
 *   doesn't plug directly into Bun.serve.
 *
 *   Therefore we use shape (b): Bun-native fetch routing.
 *   • /rpc/* — handled by Connect's universalHandler extracted from
 *     connectNodeAdapter's router, bridged through a thin fetch adapter.
 *   • everything else — delegated to Hono's `app.fetch`.
 *
 *   The Connect seam is ~40 lines using connectNodeAdapter behind a
 *   node:http IncomingMessage/ServerResponse shim — but Bun.serve gives us
 *   a Request/Response interface, so we use `universalRequestFromFetch` from
 *   @connectrpc/connect's protocol layer instead. Since @connectrpc/connect-node
 *   v2 doesn't export a universal-fetch bridge, we bridge through createServer
 *   from node:http which Bun DOES support.
 *
 *   FINAL SHAPE: single Bun.serve whose `fetch` handler:
 *     1. If path starts with /rpc/ → forward to a node:http server bound to
 *        an in-process socket (the Connect adapter runs there).
 *     2. Otherwise → Hono app.fetch.
 *
 *   SIMPLER alternative actually chosen: since Bun supports node:http fully,
 *   we start the node:http server (with the Connect adapter) + Hono side-by-side
 *   on the SAME port using Bun's node:http. The node:http server uses
 *   connectNodeAdapter with a `fallback` that delegates to Hono via
 *   universalResponseToNodeResponse-compatible shim.
 *
 *   ACTUAL FINAL SHAPE (simplest that works under Bun 1.3):
 *   A single node:http createServer where:
 *     - /rpc/* is handled by connectNodeAdapter
 *     - everything else falls through to Hono via app.fetch wrapped in a
 *       node:http adapter (Hono provides nodeAdapter for this in @hono/node-server,
 *       but since we're on Bun we use the simpler pattern: call app.fetch with
 *       a synthetic Request and pipe the Response back).
 *
 *   This way: one port, one server, clean separation.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { Hono } from "hono";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";

export type RouteRegistrar = (router: ConnectRouter) => void;

/**
 * Build and return a node:http Server that:
 *   - routes /rpc/* to the Connect adapter (empty routes by default; later
 *     tasks register real services by passing a `routes` function)
 *   - routes everything else to the Hono app via fetch adapter
 *
 * Exported so tests can call buildServer(...) on an ephemeral port.
 */
export function buildServer(app: Hono, routes: RouteRegistrar = () => {}) {
  // The Connect adapter handles all /rpc/* requests.
  const connectHandler = connectNodeAdapter({ routes });

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const url = req.url ?? "/";

    if (url.startsWith("/rpc/") || url === "/rpc") {
      // Connect adapter takes over: handles Connect/gRPC/gRPC-Web protocols.
      connectHandler(req, res);
      return;
    }

    // Everything else: delegate to Hono.
    // Build a WHATWG Request from the node:http IncomingMessage.
    const host = req.headers["host"] ?? "localhost";
    const protocol = "http";
    const fullUrl = `${protocol}://${host}${url}`;

    const headers = new Headers();
    for (const [key, value] of Object.entries(req.headers)) {
      if (value === undefined) continue;
      if (Array.isArray(value)) {
        for (const v of value) headers.append(key, v);
      } else {
        headers.set(key, value);
      }
    }

    // Collect body (needed for POST/PUT/PATCH).
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const body =
        req.method !== "GET" && req.method !== "HEAD" && chunks.length > 0
          ? Buffer.concat(chunks)
          : null;

      const honoReq = new Request(fullUrl, {
        method: req.method ?? "GET",
        headers,
        body,
      });

      Promise.resolve(app.fetch(honoReq))
        .then((honoRes: Response) => {
          res.writeHead(honoRes.status, Object.fromEntries(honoRes.headers.entries()));
          return honoRes.arrayBuffer();
        })
        .then((buf: ArrayBuffer) => {
          res.end(Buffer.from(buf));
        })
        .catch((err: unknown) => {
          console.error("Orchestrator: unhandled error in Hono dispatch", err);
          res.writeHead(500);
          res.end("Internal Server Error");
        });
    });
  });

  return server;
}
