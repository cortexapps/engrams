/**
 * /api/v1/me/claude-token routes (ADR 0051 Task 20).
 *
 * Mirrors the coordinator's /me/claude-token handlers (principal.rs) so that
 * Task 22 can re-point the web's AuthProvider here with zero JSON-key changes.
 *
 * Routes:
 *   POST   /api/v1/me/claude-token  — { token } → putSecret(key=userId, value=token) → 204
 *   GET    /api/v1/me/claude-token  — hasSecret(key=userId)   → { has_claude_token: bool }
 *   DELETE /api/v1/me/claude-token  — deleteSecret(key=userId) → 204
 *
 * The key is ALWAYS the session's user id — never client-supplied (opaque relay;
 * the plaintext token is never logged or persisted locally in the orchestrator).
 *
 * JSON shapes mirror the coordinator's MeResponse.has_claude_token field
 * and the web's saveClaudeToken/fetchMe expectations (web/src/api.ts,
 * web/src/types.ts).
 *
 * Note: GET /api/v1/me (full principal) is out-of-scope for Task 20 — Task 22
 * wires the full AuthProvider replacement which includes fetchMe.
 *
 * Injectable deps for tests: see makeMeRoute(deps).
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import {
  secrets as defaultSecrets,
} from "../control-plane/client.ts";
import { auth } from "../auth/better-auth.ts";
import type { GetSession } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of SecretService client used by the /me routes. */
export interface SecretsClient {
  putSecret(req: { key: string; value: string }): Promise<unknown>;
  hasSecret(req: { key: string }): Promise<{ exists: boolean }>;
  deleteSecret(req: { key: string }): Promise<unknown>;
}

/** Injectable deps for the /me route. */
export interface MeDeps {
  secrets?: SecretsClient;
  getSession?: GetSession;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export function makeMeRoute(deps?: MeDeps): Hono {
  const app = new Hono();
  const secretsClient: SecretsClient =
    (deps?.secrets as SecretsClient | undefined) ??
    (defaultSecrets as unknown as SecretsClient);

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

  // POST /api/v1/me/claude-token — opaque relay to SecretService.PutSecret.
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

    // Opaque relay — never logged, never persisted locally.
    await secretsClient.putSecret({ key: user.id, value: token });

    // Mirror coordinator: 204 No Content.
    return new Response(null, { status: 204 });
  });

  // GET /api/v1/me/claude-token — { has_claude_token: bool }
  // Mirrors the has_claude_token field of MeResponse (principal.rs).
  // web/src/components/settings/TokensPanel.tsx reads principal.has_claude_token
  // after a refresh() — this endpoint provides the standalone check.
  app.get("/api/v1/me/claude-token", async (c) => {
    const user = await requireUser(c.req.raw.headers);

    const { exists } = await secretsClient.hasSecret({ key: user.id });
    return c.json({ has_claude_token: exists });
  });

  // DELETE /api/v1/me/claude-token — 204 No Content.
  app.delete("/api/v1/me/claude-token", async (c) => {
    const user = await requireUser(c.req.raw.headers);

    await secretsClient.deleteSecret({ key: user.id });

    return new Response(null, { status: 204 });
  });

  return app;
}

export default makeMeRoute();
