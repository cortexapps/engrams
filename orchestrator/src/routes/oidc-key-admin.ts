/**
 * Explicit admin trigger for OIDC signing-key rotation (ADR 0109 closeout).
 *
 * POST /api/v1/admin/integrations/oidc/rotate — rotate NOW, regardless of key
 * age: the active key becomes `retiring` (published for the overlap window)
 * and a fresh active key is created. Use it for suspected key compromise or
 * to exercise the overlap window; the scheduled policy
 * (`integrations/oidc-key-rotation.ts`) handles routine age-based rotation.
 *
 * Admin-only (browser session + CASL manage-all), mirroring routes/admin.ts.
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";

import { getSessionFromHeaders } from "../auth/session.ts";
import { abilityFor } from "../authz/ability.ts";
import {
  makeIntegrationOidcKeyStore,
  type IntegrationOidcKeyStore,
} from "../db/integration-oidc-keys.ts";
import { OIDC_KEY_PUBLISH_OVERLAP_MS } from "../integrations/oidc-key-rotation.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "oidc-key-admin" });

/** Narrow session lookup so tests inject a plain fake. */
export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export function makeOidcKeyAdminRoute(deps: {
  keys?: IntegrationOidcKeyStore;
  now?: () => Date;
  getSession?: GetSession;
} = {}): Hono {
  const app = new Hono();
  const now = deps.now ?? (() => new Date());
  const getSession = deps.getSession ?? getSessionFromHeaders;

  async function requireAdmin(headers: Headers): Promise<string> {
    const session = await getSession(headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    const ability = abilityFor({
      id: session.user.id,
      role: session.user.role ?? "user",
    });
    if (!ability.can("manage", "all")) {
      throw new HTTPException(403, { message: "admin required" });
    }
    return session.user.id;
  }

  app.post("/api/v1/admin/integrations/oidc/rotate", async (c) => {
    const adminUserId = await requireAdmin(c.req.raw.headers);
    const keys = deps.keys ?? makeIntegrationOidcKeyStore();
    const rotatedAt = now();
    const rotated = await keys.rotate(rotatedAt, OIDC_KEY_PUBLISH_OVERLAP_MS);
    log.info(
      { adminUserId, kid: rotated.kid },
      "oidc signing key rotated by admin request",
    );
    return c.json({
      kid: rotated.kid,
      publishOverlapUntil: new Date(
        rotatedAt.getTime() + OIDC_KEY_PUBLISH_OVERLAP_MS,
      ).toISOString(),
    });
  });

  return app;
}
