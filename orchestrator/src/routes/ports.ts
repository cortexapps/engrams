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
import {
  portRelay as defaultPortRelay,
  sessions as defaultSessions,
} from "../control-plane/client.ts";
import { tunnelSocket, type PortRelayClient } from "./preview-proxy.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

export interface PortsDeps {
  store?: PortExposureStore;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
  /** Override the preview base domain (default: config.previewBaseDomain). */
  previewBaseDomain?: string;
  /** PortRelay client for the liveness probe (default: control-plane singleton). */
  portRelay?: PortRelayClient;
  /** Session-status lookup for the liveness gate (default: control-plane getSession). */
  sessionStatus?: (sessionId: string) => Promise<string | null>;
}

/** How long to wait for the guest port to answer an HTTP probe before calling
 * it down. Localhost-fast on success; a dead/refused port simply never answers
 * within the window (the orchestrator-side timeout governs, regardless of the
 * host's own dial-retry behaviour). */
const LIVENESS_TIMEOUT_MS = 2_500;

/**
 * Liveness = does the guest port answer? Opens the `PortRelay` tunnel, sends a
 * minimal HTTP HEAD, and resolves `true` on the first response byte; no bytes
 * within the window (refused / not serving / dead) → `false`.
 *
 * The caller MUST have already confirmed the session is active — opening the
 * relay on an idle session would auto-resume it (`ensure_active`), which a
 * health check must never trigger.
 */
async function probeLiveness(
  relay: PortRelayClient,
  sessionId: string,
  port: number,
): Promise<boolean> {
  const ac = new AbortController();
  const tunnel = tunnelSocket(relay, sessionId, port, ac.signal);
  return await new Promise<boolean>((resolve) => {
    let settled = false;
    const finish = (up: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      ac.abort(); // tear down the relay stream
      tunnel.destroy();
      resolve(up);
    };
    const timer = setTimeout(() => finish(false), LIVENESS_TIMEOUT_MS);
    tunnel.on("data", () => finish(true));
    tunnel.on("error", () => finish(false));
    tunnel.on("end", () => finish(false));
    tunnel.write("HEAD / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n");
  });
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
  const relay: PortRelayClient = deps?.portRelay ?? defaultPortRelay;
  const sessionStatus =
    deps?.sessionStatus ??
    (async (id: string): Promise<string | null> => {
      const r = await defaultSessions.getSession({ sessionId: id });
      return r.session?.status ?? null;
    });

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

  // ---- GET: liveness ----
  // "Is the guest port answering?" for one exposure. Gated on the session being
  // active: a suspended session has no live port, and opening the relay would
  // auto-resume it — which a health check must never do. Non-active → "unknown"
  // (honest, and silent in the UI). Returns 404 for a slug not on this session.
  app.get("/api/v1/sessions/:id/ports/:slug/health", async (c) => {
    await guardFn(c, "read");
    const sessionId = c.req.param("id");
    const slug = c.req.param("slug");

    const row = await getStore().getBySlug(slug);
    if (!row || row.sessionId !== sessionId) {
      return c.json({ error: "not found" }, 404);
    }

    if ((await sessionStatus(sessionId)) !== "active") {
      return c.json({ status: "unknown" as const });
    }

    const up = await probeLiveness(relay, sessionId, row.port);
    return c.json({ status: up ? ("up" as const) : ("down" as const) });
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
