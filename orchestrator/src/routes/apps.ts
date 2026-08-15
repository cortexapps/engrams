/**
 * Session-app CRUD route (ADR 0118, replacing ADR 0064's ports route).
 *
 *   POST   /api/v1/sessions/:id/apps                    { port, name?, visibility? }
 *   GET    /api/v1/sessions/:id/apps                    → list this session's apps
 *   GET    /api/v1/sessions/:id/apps/:hostLabel/health  → is the guest port answering?
 *   DELETE /api/v1/sessions/:id/apps/:hostLabel         → revoke one app
 *
 * The declarative path (profile.apps, reserved before the session is created)
 * is the primary one; this route is the ad-hoc "expose what I just started"
 * surface. An ad-hoc exposure IS an app — omitting `name` derives `port-<port>`
 * — so the product keeps the capability with one model instead of two.
 *
 * An app added here gets no env var: the guest's environment is fixed when the
 * harness binds, and a process that is already running cannot be told about a
 * name that did not exist when it started. Declare the app on the profile if a
 * sibling has to reach it.
 *
 * Auth: the same ownership gate as the rest of the session-scoped surface
 * (guard.ts). Mutations use the `shell` action — publishing a guest port is the
 * same live-interactive-access bar as opening the shell; listing uses `read`.
 *
 * All deps are injectable for tests (see makeAppsRoute(deps)).
 */

import { Hono } from "hono";
import { config } from "../config.ts";
import {
  makeSessionAppStore,
  type SessionAppRow,
  type SessionAppStore,
  type Visibility,
} from "../db/session-apps.ts";
import {
  portRelay as defaultPortRelay,
  sessions as defaultSessions,
} from "../control-plane/client.ts";
import { tunnelSocket, type PortRelayClient } from "./preview-proxy.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { appUrl, defaultAppName, isValidAppName } from "../apps/hostname.ts";

export interface AppsDeps {
  store?: SessionAppStore;
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

function isValidVisibility(v: unknown): v is Visibility {
  return v === "org" || v === "private";
}

export function makeAppsRoute(deps?: AppsDeps): Hono {
  const app = new Hono();
  // Resolve the store lazily so the module-level default export doesn't call
  // getDb() at import time (it throws without a DB URL — e.g. in unit tests).
  let store = deps?.store;
  const getStore = (): SessionAppStore => (store ??= makeSessionAppStore());
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);
  const baseDomain = deps?.previewBaseDomain ?? config.previewBaseDomain;
  const relay: PortRelayClient = deps?.portRelay ?? defaultPortRelay;
  const sessionStatus =
    deps?.sessionStatus ??
    (async (id: string): Promise<string | null> => {
      const r = await defaultSessions.getSession({ sessionId: id });
      return r.session?.status ?? null;
    });

  /** Shape a row for the wire — adds the public URL. */
  const toJson = (r: SessionAppRow) => ({
    hostLabel: r.hostLabel,
    sessionId: r.sessionId,
    name: r.name,
    port: r.port,
    visibility: r.visibility,
    url: appUrl(r.hostLabel, baseDomain),
    createdAt: r.createdAt,
  });

  // ---- POST: reserve / re-reserve ----
  app.post("/api/v1/sessions/:id/apps", async (c) => {
    const user = await guardFn(c, "shell");
    const sessionId = c.req.param("id");

    const body = (await c.req.json().catch(() => null)) as {
      port?: unknown;
      name?: unknown;
      visibility?: unknown;
    } | null;

    const port = typeof body?.port === "number" ? body.port : NaN;
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return c.json({ error: "port must be an integer in 1..=65535" }, 400);
    }
    const name =
      typeof body?.name === "string" && body.name.trim() !== ""
        ? body.name.trim().toLowerCase()
        : defaultAppName(port);
    if (!isValidAppName(name)) {
      return c.json(
        { error: "name must be 1-24 lowercase alphanumeric characters or interior hyphens" },
        400,
      );
    }
    const visibility: Visibility = isValidVisibility(body?.visibility)
      ? body.visibility
      : "org";

    const row = await getStore().createOne(sessionId, user.id, { name, port, visibility });
    return c.json(toJson(row), 201);
  });

  // ---- GET: list ----
  app.get("/api/v1/sessions/:id/apps", async (c) => {
    await guardFn(c, "read");
    const sessionId = c.req.param("id");
    const rows = await getStore().listBySession(sessionId);
    return c.json({ apps: rows.map(toJson) });
  });

  // ---- GET: liveness ----
  // "Is the guest port answering?" for one app. Gated on the session being
  // active: a suspended session has no live port, and opening the relay would
  // auto-resume it — which a health check must never do. Non-active → "unknown"
  // (honest, and silent in the UI). Returns 404 for a label not on this session.
  app.get("/api/v1/sessions/:id/apps/:hostLabel/health", async (c) => {
    await guardFn(c, "read");
    const sessionId = c.req.param("id");
    const hostLabel = c.req.param("hostLabel");

    const row = await getStore().getByHostLabel(hostLabel);
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
  app.delete("/api/v1/sessions/:id/apps/:hostLabel", async (c) => {
    await guardFn(c, "shell");
    const sessionId = c.req.param("id");
    const hostLabel = c.req.param("hostLabel");

    // Anti-cross-session: the label must belong to THIS session (404 otherwise,
    // matching the guard's anti-enumeration shape).
    const row = await getStore().getByHostLabel(hostLabel);
    if (!row || row.sessionId !== sessionId) {
      return c.json({ error: "not found" }, 404);
    }
    await getStore().deleteByHostLabel(hostLabel);
    return c.body(null, 204);
  });

  return app;
}

export default makeAppsRoute();
