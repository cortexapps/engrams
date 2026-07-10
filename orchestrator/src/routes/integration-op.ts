/**
 * Admin trigger for the IntegrationOp seam (testability).
 *
 *   POST /api/v1/integrations/:provider/run-op
 *   { method, path, body?, contentType? }  — body may be a JSON object or a string
 *
 * Fires the same {@link runIntegrationOp} primitive the typed SDK clients use, so a
 * sessionless integration call can be exercised by hand (and asserted in tests).
 * Admin-only; the credential is resolved + applied coordinator-side.
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import { getSessionFromHeaders } from "../auth/session.ts";
import { abilityFor } from "../authz/ability.ts";
import { runIntegrationOp, type RunOpDeps } from "../integrations/run-op.ts";

export function makeIntegrationOpRoute(deps?: RunOpDeps): Hono {
  const app = new Hono();

  async function requireAdmin(headers: Headers): Promise<void> {
    const session = await getSessionFromHeaders(headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    const ability = abilityFor({ id: session.user.id, role: session.user.role ?? "user" });
    if (!ability.can("manage", "all")) throw new HTTPException(403, { message: "admin required" });
  }

  app.post("/api/v1/integrations/:provider/run-op", async (c) => {
    await requireAdmin(c.req.raw.headers);
    const provider = c.req.param("provider");
    let req: { method?: string; path?: string; body?: unknown; contentType?: string };
    try {
      req = await c.req.json();
    } catch {
      throw new HTTPException(400, { message: "body must be JSON" });
    }
    if (!req.method || !req.path) {
      throw new HTTPException(400, { message: '"method" and "path" are required' });
    }
    // A JSON object body is serialized; a string is sent verbatim.
    const body =
      req.body === undefined
        ? undefined
        : typeof req.body === "string"
          ? req.body
          : JSON.stringify(req.body);
    try {
      const result = await runIntegrationOp(
        provider,
        { method: req.method, path: req.path, body, contentType: req.contentType },
        deps,
      );
      return c.json({
        status: result.status,
        truncated: result.truncated,
        contentType: result.contentType,
        body: new TextDecoder().decode(result.body),
      });
    } catch (e) {
      throw new HTTPException(502, { message: (e as Error).message });
    }
  });

  return app;
}

export default makeIntegrationOpRoute();
