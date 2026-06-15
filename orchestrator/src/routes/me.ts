/**
 * /api/v1/me/claude-token routes (ADR 0051).
 *
 * The orchestrator OWNS the user's Claude harness token in its OWN Postgres
 * (the `user_harness_token` table) — replacing the coordinator's per-user
 * sealed SecretService vault (Drip A). At session-create the token is resolved
 * and passed to the control plane via `CreateSession.harness_env`.
 *
 * Routes:
 *   POST   /api/v1/me/claude-token  — { token } → store.put(userId, token) → 204
 *   GET    /api/v1/me/claude-token  — store.has(userId)    → { has_claude_token: bool }
 *   DELETE /api/v1/me/claude-token  — store.delete(userId) → 204
 *
 * The key is ALWAYS the session's user id — never client-supplied. The token
 * plaintext is NEVER logged.
 *
 * JSON shapes mirror the coordinator's MeResponse.has_claude_token field and
 * the web's saveClaudeToken/fetchMe expectations (web/src/api.ts,
 * web/src/types.ts).
 *
 * Injectable deps for tests: see makeMeRoute(deps).
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import {
  makeHarnessTokenStore,
  type HarnessTokenStore,
} from "../db/harness-token.ts";
import { auth } from "../auth/better-auth.ts";
import type { GetSession } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Injectable deps for the /me route. */
export interface MeDeps {
  tokens?: HarnessTokenStore;
  getSession?: GetSession;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export function makeMeRoute(deps?: MeDeps): Hono {
  const app = new Hono();
  // The store is constructed lazily by default so importing this module does
  // not require ORCHESTRATOR_DATABASE_URL at import time (tests inject a fake).
  const resolveStore = (): HarnessTokenStore =>
    deps?.tokens ?? makeHarnessTokenStore();

  const resolveSession: GetSession =
    deps?.getSession ??
    ((headers) =>
      auth.api.getSession({
        headers,
      } as Parameters<typeof auth.api.getSession>[0]));

  /** Resolve the authenticated user or throw 401. */
  async function requireUser(
    headers: Headers,
  ): Promise<{ id: string; role: string }> {
    const session = await resolveSession(headers);
    if (!session) {
      throw new HTTPException(401, { message: "unauthenticated" });
    }
    return { id: session.user.id, role: session.user.role ?? "user" };
  }

  // POST /api/v1/me/claude-token — upsert the user's token in our own store.
  // body: { token: string }
  // response: 204 No Content (mirrors coordinator save_claude_token)
  app.post("/api/v1/me/claude-token", async (c) => {
    const user = await requireUser(c.req.raw.headers);

    let body: { token?: unknown };
    try {
      body = (await c.req.json()) as { token?: unknown };
    } catch {
      throw new HTTPException(400, { message: "invalid JSON body" });
    }

    const token =
      typeof body.token === "string" ? body.token.trim() : undefined;
    if (!token) {
      throw new HTTPException(400, { message: "token must not be empty" });
    }

    // NEVER log the token.
    await resolveStore().put(user.id, token);

    // Mirror coordinator: 204 No Content.
    return new Response(null, { status: 204 });
  });

  // GET /api/v1/me/claude-token — { has_claude_token: bool }
  // Mirrors the has_claude_token field of MeResponse (principal.rs).
  app.get("/api/v1/me/claude-token", async (c) => {
    const user = await requireUser(c.req.raw.headers);

    const exists = await resolveStore().has(user.id);
    return c.json({ has_claude_token: exists });
  });

  // DELETE /api/v1/me/claude-token — 204 No Content.
  app.delete("/api/v1/me/claude-token", async (c) => {
    const user = await requireUser(c.req.raw.headers);

    await resolveStore().delete(user.id);

    return new Response(null, { status: 204 });
  });

  return app;
}

export default makeMeRoute();
