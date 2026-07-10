/**
 * Centralized session resolution (ADR 0086).
 *
 * Every inbound auth seam (Connect passthrough gate, native services, Hono
 * guards) resolves identity through this ONE wrapper instead of calling
 * `auth.api.getSession` inline.
 *
 * Why the try/catch: the @better-auth/api-key plugin's session hook THROWS an
 * APIError for an invalid, expired, or disabled key rather than returning
 * null. Uncaught, that surfaces as 500/Code.Unknown at every seam; caught
 * here, a bad key is simply anonymous and each seam fails closed with its
 * normal Unauthenticated/401. The flip side: a genuine better-auth internal
 * failure also reads as anonymous (mass 401s, not 500s) — the same posture
 * the guard and IAP bridge already take.
 */

import { auth } from "./better-auth.ts";

/** The `{ user: { id, role, … }, session }` shape (or null) all seams consume. */
export type ResolvedSession = Awaited<ReturnType<typeof auth.api.getSession>>;

export async function getSessionFromHeaders(headers: Headers): Promise<ResolvedSession> {
  try {
    // better-auth.api.getSession accepts HeadersInit; a real Headers instance
    // satisfies it — the cast only bridges better-auth's request-typed param.
    return await auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]);
  } catch {
    return null;
  }
}
