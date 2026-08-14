import { Hono } from "hono";

import { sessions as defaultSessions } from "../control-plane/client.ts";
import { type SessionsClient, streamSessionEventsSSE } from "./events.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecEventsRouteDeps {
  resolveMembership: ResolveSpecMembership;
  resolveSessionId(specId: string): Promise<string | null>;
  sessions?: SessionsClient;
  getSession?: GetSession;
}

export function makeSpecEventsRoute(deps: SpecEventsRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
  const sessionsClient = deps.sessions ?? (defaultSessions as unknown as SessionsClient);

  app.get("/api/v1/specs/:id/events", async (c) => {
    const specId = c.req.param("id");
    if (!UUID.test(specId)) return c.json({ error: "not found" }, 404);
    const access = await authorize(c.req.raw.headers, specId);
    if (!access.ok) {
      return c.json(
        { error: access.status === 401 ? "unauthenticated" : "not found" },
        access.status,
      );
    }
    const sessionId = await deps.resolveSessionId(specId);
    if (!sessionId) return c.json({ error: "not found" }, 404);
    return streamSessionEventsSSE(c, sessionId, sessionsClient);
  });

  return app;
}
