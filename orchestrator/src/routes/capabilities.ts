/**
 * Session capabilities (ADR 0064).
 *
 * GET /api/v1/sessions/:id/capabilities → { browserEnabled }
 *
 * Owner-gated with the same guard as /shell and /vnc (action "read" — the
 * session owner may read). `browserEnabled` is true when the session's profile
 * selected the `browser` skill bundle (ADR 0055 profile.skills, ADR 0064 P0.1).
 * Resolving the capability here (not in the coordinator) keeps the
 * task→profile→skills mapping orchestrator-owned.
 */
import { Hono } from "hono";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { resolveSessionProfileSkills } from "../authz/resolve.ts";

export interface CapabilitiesDeps {
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
  resolveSkills?: (sessionId: string) => Promise<string[]>;
}

export function makeCapabilitiesRoute(deps?: CapabilitiesDeps): Hono {
  const app = new Hono();
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);
  const resolveSkills = deps?.resolveSkills ?? resolveSessionProfileSkills;

  app.get("/api/v1/sessions/:id/capabilities", async (c) => {
    await guardFn(c, "read"); // throws HTTPException (401/404) on failure
    const skills = await resolveSkills(c.req.param("id"));
    return c.json({ browserEnabled: skills.includes("browser") });
  });

  return app;
}
