/**
 * Port-exposure CRUD route (ADR 0064 P2a).
 *
 *   POST   /api/v1/sessions/:id/ports        { port, label?, visibility? }  → mint/return a slug
 *   GET    /api/v1/sessions/:id/ports                                       → list this session's exposures
 *   DELETE /api/v1/sessions/:id/ports/:slug                                 → revoke one exposure
 *
 * Auth: the same ownership gate as the rest of the session-scoped surface
 * (guard.ts). Mutations use the `shell` action — exposing a guest port is the
 * same live-interactive-access bar as opening the shell — so an owner (or an
 * admin via `manage('all')`) may create/revoke; listing uses `read`.
 *
 * This phase only manages the registry; the edge reverse-proxy that actually
 * serves `<slug>.<previewBaseDomain>` over PortRelayService lands in P2b. The
 * returned `url` is therefore the eventual address, not yet reachable.
 *
 * All deps are injectable for tests (see makePortsRoute(deps)).
 */

import { Hono } from "hono";
import { config } from "../config.ts";
import {
  makePortExposureStore,
  type PortExposureRow,
  type PortExposureStore,
  type Visibility,
} from "../db/port-exposures.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

export interface PortsDeps {
  store?: PortExposureStore;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
  /** Override the preview base domain (default: config.previewBaseDomain). */
  previewBaseDomain?: string;
}

/** Local-dev preview domains are served over plain http; everything else https. */
function schemeFor(domain: string): "http" | "https" {
  return /(localhost|127\.0\.0\.1|lvh\.me|localtest\.me)/.test(domain) ? "http" : "https";
}

function isValidVisibility(v: unknown): v is Visibility {
  return v === "private" || v === "shared";
}

export function makePortsRoute(deps?: PortsDeps): Hono {
  const app = new Hono();
  // Resolve the store lazily so the module-level default export doesn't call
  // getDb() at import time (it throws without a DB URL — e.g. in unit tests).
  let store = deps?.store;
  const getStore = (): PortExposureStore => (store ??= makePortExposureStore());
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);
  const baseDomain = deps?.previewBaseDomain ?? config.previewBaseDomain;
  const scheme = schemeFor(baseDomain);

  const urlFor = (slug: string): string => `${scheme}://${slug}.${baseDomain}`;

  /** Shape a row for the wire — adds the (eventual) preview URL. */
  const toJson = (r: PortExposureRow) => ({
    slug: r.slug,
    sessionId: r.sessionId,
    port: r.port,
    label: r.label,
    visibility: r.visibility,
    shareToken: r.shareToken,
    url: urlFor(r.slug),
    createdAt: r.createdAt,
    expiresAt: r.expiresAt,
  });

  // ---- POST: create / re-expose ----
  app.post("/api/v1/sessions/:id/ports", async (c) => {
    const user = await guardFn(c, "shell");
    const sessionId = c.req.param("id");

    const body = (await c.req.json().catch(() => null)) as {
      port?: unknown;
      label?: unknown;
      visibility?: unknown;
    } | null;

    const port = typeof body?.port === "number" ? body.port : NaN;
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return c.json({ error: "port must be an integer in 1..=65535" }, 400);
    }
    const label = typeof body?.label === "string" ? body.label.slice(0, 200) : "";
    const visibility: Visibility = isValidVisibility(body?.visibility)
      ? body.visibility
      : "private";

    const row = await getStore().createOrGet({
      sessionId,
      port,
      label,
      ownerUserId: user.id,
      visibility,
    });
    return c.json(toJson(row), 201);
  });

  // ---- GET: list ----
  app.get("/api/v1/sessions/:id/ports", async (c) => {
    await guardFn(c, "read");
    const sessionId = c.req.param("id");
    const rows = await getStore().listBySession(sessionId);
    return c.json({ exposures: rows.map(toJson) });
  });

  // ---- DELETE: revoke ----
  app.delete("/api/v1/sessions/:id/ports/:slug", async (c) => {
    await guardFn(c, "shell");
    const sessionId = c.req.param("id");
    const slug = c.req.param("slug");

    // Anti-cross-session: the slug must belong to THIS session (404 otherwise,
    // matching the guard's anti-enumeration shape).
    const row = await getStore().getBySlug(slug);
    if (!row || row.sessionId !== sessionId) {
      return c.json({ error: "not found" }, 404);
    }
    await getStore().deleteBySlug(slug);
    return c.body(null, 204);
  });

  return app;
}

export default makePortsRoute();
