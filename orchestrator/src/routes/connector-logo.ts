/**
 * Connector-logo serve route (redesign).
 *
 * GET /api/v1/integrations/:provider/logo
 *
 * Orchestrator-owned (the coordinator never sees connectors). Streams the bytes
 * from the `connector_logo` table; rendered by the web via `<img src>` (so an
 * uploaded SVG can't execute script) over the always-present monogram. A 404 ⇒
 * the web falls back to the deterministic monogram. Member-readable: any
 * authenticated user (same-origin cookie auth rides the <img> request).
 *
 * Injectable deps (store, getSession) for tests.
 */

import { Hono } from "hono";

import { getSessionFromHeaders } from "../auth/session.ts";
import { makeConnectorLogoStore, type ConnectorLogoStore } from "../db/connector-logos.ts";

export type GetSession = (headers: Headers) => Promise<{ user: unknown } | null>;

export interface ConnectorLogoRouteDeps {
  store?: ConnectorLogoStore;
  getSession?: GetSession;
}

export function makeConnectorLogoRoute(deps?: ConnectorLogoRouteDeps): Hono {
  const app = new Hono();
  // Resolve the store lazily so the module-level default export doesn't call
  // getDb() at import time (it throws without a DB URL — e.g. in unit tests).
  let store = deps?.store;
  const getStore = (): ConnectorLogoStore => (store ??= makeConnectorLogoStore());
  const getSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;

  app.get("/api/v1/integrations/:provider/logo", async (c) => {
    const session = await getSession(c.req.raw.headers);
    if (!session) return c.json({ error: "unauthenticated" }, 401);

    const row = await getStore().get(c.req.param("provider"));
    if (!row) return c.json({ error: "no logo" }, 404);

    // Copy into a standalone ArrayBuffer (an unambiguous BodyInit; node's
    // Buffer<ArrayBufferLike> doesn't satisfy the DOM body types directly).
    const body = row.data.buffer.slice(
      row.data.byteOffset,
      row.data.byteOffset + row.data.byteLength,
    ) as ArrayBuffer;
    return new Response(body, {
      headers: {
        "Content-Type": row.mediaType,
        // Logos can be replaced; keep a short cache so a rotation shows up soon.
        "Cache-Control": "private, max-age=300",
        // Defense in depth: never let the browser content-sniff the bytes.
        "X-Content-Type-Options": "nosniff",
      },
    });
  });

  return app;
}

export default makeConnectorLogoRoute();
