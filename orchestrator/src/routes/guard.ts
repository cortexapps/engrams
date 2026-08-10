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
import { getSessionFromHeaders } from "../auth/session.ts";
import type { Actions } from "../authz/ability.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Injected getSession implementation (same shape as passthrough.ts). */
export type GetSession = (headers: Headers) => Promise<{
  user: { id: string; role?: string | null; email?: string | null; name?: string | null };
} | null>;

/** Injected session-owner resolver. */
export type ResolveOwner = (sessionId: string) => Promise<string | null>;

/** Resolve whether a user is a member of the organization that owns a spec. */
export type ResolveSpecMembership = (specId: string, userId: string) => Promise<boolean>;

/** Resolve whether a spec still accepts live document updates. */
export type ResolveDraftSpec = (specId: string) => Promise<boolean>;

/** Resolved user returned by the guard — carries id + role. */
export interface GuardUser {
  id: string;
  role: string;
  name?: string;
}

/** Outcome of a header-level authorization check (no Hono `Context` involved —
 * usable from both HTTP handlers and raw `node:http` upgrade handlers). */
export type GuardResult = { ok: true; user: GuardUser } | { ok: false; status: 401 | 404 };

// ---------------------------------------------------------------------------
// Pure authorization check
// ---------------------------------------------------------------------------

/**
 * Resolve + authorize a session-scoped request from raw `Headers` — the
 * mechanics shared by the Hono-`Context` guard below (`makeGuard`) and the
 * path-keyed WS upgrade hooks (e.g. `routes/ide.ts`) that run before a Hono
 * `Context` exists:
 *   1. Resolve the better-auth session from `headers` → 401 if absent.
 *   2. Resolve the session owner via DB (`resolveOwner`) → 404 if unknown.
 *   3. Check `ability.can(action, subject('Session', { createdByUserId: owner }))` → 404 if denied.
 *
 * Anti-enumeration: unknown and unowned sessions both map to 404.
 */
export async function authorizeSessionAccess(
  headers: Headers,
  sessionId: string | undefined,
  action: Actions,
  resolveSession: GetSession,
  ownerResolver: ResolveOwner,
): Promise<GuardResult> {
  const session = await resolveSession(headers);
  if (!session) return { ok: false, status: 401 };

  const user: GuardUser = {
    id: session.user.id,
    role: session.user.role ?? "user",
    ...(session.user.name ? { name: session.user.name } : {}),
  };

  if (!sessionId) return { ok: false, status: 404 };

  const ability = abilityFor(user);
  const ownerId = await ownerResolver(sessionId);
  if (!ability.can(action, subject("Session", { createdByUserId: ownerId }))) {
    return { ok: false, status: 404 };
  }

  return { ok: true, user };
}

/**
 * Authorize an organization member for a spec-shared surface.
 *
 * A spec is the first live surface that is shared with all organization
 * members. This check is separate from the session-owner check so a caller
 * cannot accidentally apply owner-only policy to the collaborative document.
 */
export async function authorizeSpecMemberAccess(
  headers: Headers,
  specId: string | undefined,
  resolveSession: GetSession,
  resolveMembership: ResolveSpecMembership,
): Promise<GuardResult> {
  const session = await resolveSession(headers);
  if (!session) return { ok: false, status: 401 };

  const user: GuardUser = {
    id: session.user.id,
    role: session.user.role ?? "user",
    ...(session.user.name ? { name: session.user.name } : {}),
  };

  if (!specId || !(await resolveMembership(specId, user.id))) {
    return { ok: false, status: 404 };
  }

  return { ok: true, user };
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

/** Resolve the default (real) resolvers, honoring injected overrides. */
function resolveDefaults(
  getSession?: GetSession,
  resolveOwner?: ResolveOwner,
): { resolveSession: GetSession; ownerResolver: ResolveOwner } {
  const resolveSession: GetSession = getSession ?? getSessionFromHeaders;
  const ownerResolver: ResolveOwner = resolveOwner ?? resolveSessionOwner;
  return { resolveSession, ownerResolver };
}

/**
 * Create a header-level guard function with optional injectable deps (for
 * tests) — same authorization as `makeGuard`, but callable before a Hono
 * `Context` exists (raw `node:http` upgrade handlers).
 */
export function makeHeaderGuard(getSession?: GetSession, resolveOwner?: ResolveOwner) {
  const { resolveSession, ownerResolver } = resolveDefaults(getSession, resolveOwner);
  return (headers: Headers, sessionId: string | undefined, action: Actions) =>
    authorizeSessionAccess(headers, sessionId, action, resolveSession, ownerResolver);
}

/** Create the raw-header guard for an organization-shared spec surface. */
export function makeSpecMemberHeaderGuard(
  resolveMembership: ResolveSpecMembership,
  getSession?: GetSession,
) {
  const resolveSession = getSession ?? getSessionFromHeaders;
  return (headers: Headers, specId: string | undefined) =>
    authorizeSpecMemberAccess(headers, specId, resolveSession, resolveMembership);
}

/**
 * Create a guard function with optional injectable deps (for tests).
 *
 * Returns the resolved user on success; throws HTTPException (401/404) on failure.
 *
 * The caller is responsible for extracting `:id` from the route params and
 * passing it as `sessionId`.
 */
export function makeGuard(getSession?: GetSession, resolveOwner?: ResolveOwner) {
  const headerGuard = makeHeaderGuard(getSession, resolveOwner);

  return async function guardSession(c: Context, action: Actions): Promise<GuardUser> {
    const sessionId = c.req.param("id");
    // Pull headers from the raw fetch Request (Hono wraps it).
    const result = await headerGuard(c.req.raw.headers, sessionId, action);
    if (!result.ok) {
      throw new HTTPException(result.status, {
        message: result.status === 401 ? "unauthenticated" : "not found",
      });
    }
    return result.user;
  };
}

/** Default guard instance — uses real better-auth + DB resolver. */
export const guardSession = makeGuard();
