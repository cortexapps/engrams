/**
 * Shared Hono route guard (ADR 0051 Task 20).
 *
 * Resolves the better-auth session from a Hono Context and performs the
 * CASL ownership check for session-scoped HTTP routes (SSE events,
 * artifact bytes, shell WS).
 *
 * Error shape mirrors the passthrough gate:
 *   - No session               → 401 (Unauthenticated)
 *   - Session not owned / unknown → 404 (anti-enumeration, ADR §6)
 *
 * All deps are injectable for tests; real singletons are used when omitted.
 *
 * Usage:
 *   const guard = makeGuard();
 *   const user = await guard(c, 'read');  // throws HTTPException on failure
 */

import type { Context } from "hono";
import { HTTPException } from "hono/http-exception";
import { subject } from "@casl/ability";
import { abilityFor } from "../authz/ability.ts";
import { resolveSessionOwner } from "../authz/resolve.ts";
import { auth } from "../auth/better-auth.ts";
import type { Actions } from "../authz/ability.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Injected getSession implementation (same shape as passthrough.ts). */
export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

/** Injected session-owner resolver. */
export type ResolveOwner = (sessionId: string) => Promise<string | null>;

/** Resolved user returned by the guard — carries id + role. */
export interface GuardUser {
  id: string;
  role: string;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * Create a guard function with optional injectable deps (for tests).
 *
 * The guard function:
 *   1. Resolves the better-auth session from `c.req.raw.headers` → 401 if absent.
 *   2. Resolves the session owner via DB (resolveOwner) → 404 if null.
 *   3. Checks `ability.can(action, subject('Session', { createdByUserId: owner }))` → 404 if denied.
 *
 * Returns the resolved user on success; throws HTTPException (401/404) on failure.
 *
 * The caller is responsible for extracting `:id` from the route params and
 * passing it as `sessionId`.
 */
export function makeGuard(
  getSession?: GetSession,
  resolveOwner?: ResolveOwner,
) {
  const resolveSession: GetSession =
    getSession ??
    ((headers) =>
      auth.api.getSession({
        headers,
      } as Parameters<typeof auth.api.getSession>[0]));

  const ownerResolver: ResolveOwner = resolveOwner ?? resolveSessionOwner;

  return async function guardSession(
    c: Context,
    action: Actions,
  ): Promise<GuardUser> {
    // 1. Authenticate — pull headers from the raw fetch Request (Hono wraps it).
    const session = await resolveSession(c.req.raw.headers);
    if (!session) {
      throw new HTTPException(401, { message: "unauthenticated" });
    }

    const user: GuardUser = {
      id: session.user.id,
      role: session.user.role ?? "user",
    };

    // 2. Resolve session ownership.
    const sessionId = c.req.param("id");
    if (!sessionId) {
      throw new HTTPException(404, { message: "not found" });
    }

    const ability = abilityFor(user);

    const ownerId = await ownerResolver(sessionId);

    // Anti-enumeration: unknown OR unowned session → 404.
    if (
      !ability.can(
        action,
        subject("Session", { createdByUserId: ownerId }),
      )
    ) {
      throw new HTTPException(404, { message: "not found" });
    }

    return user;
  };
}

/** Default guard instance — uses real better-auth + DB resolver. */
export const guardSession = makeGuard();
