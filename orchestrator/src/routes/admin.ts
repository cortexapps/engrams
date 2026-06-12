/**
 * Admin REST proxy routes — thin forwarding of admin-only session operations
 * that have no gRPC equivalent in the current proto.
 *
 * ADR 0039 Task 28: When the Vite proxy flips all /api browser traffic to
 * the orchestrator, these coordinator-only admin REST routes would 404. This
 * module adds orchestrator-side stubs that:
 *   1. Authenticate via the browser session cookie (better-auth).
 *   2. Assert admin role (CASL 'manage all').
 *   3. Forward the request to the coordinator REST API with the service
 *      bearer token (CONTROL_PLANE_BEARER).
 *
 * Routes:
 *   POST /api/v1/admin/sessions/:id/pause    → coordinator pause (VM freeze)
 *   POST /api/v1/admin/sessions/:id/resume   → coordinator resume (VM unfreeze)
 *
 * These are kept as REST proxies rather than gRPC calls because PauseSession /
 * ResumeSession (in-place VM freeze/unfreeze — distinct from the
 * SessionService.Resume idle-rehydrate) have no proto methods yet. When they
 * are promoted to gRPC in a future task, delete these routes and migrate the
 * hooks to connect-query.
 *
 * Note: teleportSession uses FleetService.EvacuateSession (gRPC, already
 * passing through the CASL gate) — no proxy route needed for it.
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import { auth } from "../auth/better-auth.ts";
import { abilityFor } from "../authz/ability.ts";
import { config } from "../config.ts";

export function makeAdminRoute(
  deps: { httpUrl?: string; bearer?: string } = {},
): Hono {
  const app = new Hono();
  const httpBase = (deps.httpUrl ?? config.controlPlaneHttpUrl).replace(/\/$/, "");
  const bearer = deps.bearer ?? config.controlPlaneBearer;

  /** Require admin; throws HTTPException(401/403) otherwise. */
  async function requireAdmin(headers: Headers): Promise<void> {
    const session = await auth.api.getSession({
      headers,
    } as Parameters<typeof auth.api.getSession>[0]);
    if (!session) {
      throw new HTTPException(401, { message: "unauthenticated" });
    }
    const ability = abilityFor({
      id: session.user.id,
      role: session.user.role ?? "user",
    });
    if (!ability.can("manage", "all")) {
      throw new HTTPException(403, { message: "admin required" });
    }
  }

  /** Forward a POST to the coordinator REST API. */
  async function forwardPost(
    path: string,
    body: unknown,
  ): Promise<Response> {
    const res = await fetch(`${httpBase}${path}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${bearer}`,
      },
      body: JSON.stringify(body),
    });
    return res;
  }

  // POST /api/v1/admin/sessions/:id/pause — freeze the session's microVM in place.
  // Admin-only. The session stays `active`; the guest vCPUs stop until resumed.
  app.post("/api/v1/admin/sessions/:id/pause", async (c) => {
    await requireAdmin(c.req.raw.headers);
    const id = c.req.param("id");
    const upstream = await forwardPost(`/api/v1/admin/sessions/${id}/pause`, {});
    const body = upstream.status === 204 ? null : await upstream.text();
    return new Response(body, {
      status: upstream.status,
      headers: { "Content-Type": "application/json" },
    });
  });

  // POST /api/v1/admin/sessions/:id/resume — unfreeze the session's microVM.
  // Admin-only. Counterpart to /pause; the in-place unfreeze, NOT the
  // idle-rehydrate at /sessions/:id/resume (which is SessionService.Resume).
  app.post("/api/v1/admin/sessions/:id/resume", async (c) => {
    await requireAdmin(c.req.raw.headers);
    const id = c.req.param("id");
    const upstream = await forwardPost(`/api/v1/admin/sessions/${id}/resume`, {});
    const body = upstream.status === 204 ? null : await upstream.text();
    return new Response(body, {
      status: upstream.status,
      headers: { "Content-Type": "application/json" },
    });
  });

  return app;
}

export default makeAdminRoute();
