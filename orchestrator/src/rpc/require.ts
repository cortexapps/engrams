/**
 * Shared Connect-RPC identity gates — the RPC-side counterpart of
 * routes/guard.ts. Native services resolve identity through ADR 0086's
 * single seam (getSessionFromHeaders) via an injectable GetSession;
 * these helpers are the two gate shapes every service repeats.
 *
 * Service-specific gates (e.g. api-key.ts's requireHumanUser) stay in
 * their service module.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import type { HandlerContext } from "@connectrpc/connect";

import { getSessionFromHeaders } from "../auth/session.ts";
import { isServiceAccountEmail } from "./api-key.ts";

/** Injected getSession implementation (same shape as passthrough.ts). */
export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

export interface RpcUser {
  id: string;
  role: string;
  /** A global API key resolves to its service-account owner (ADR 0086) —
   * a PROGRAMMATIC principal with no per-user harness token. */
  serviceAccount: boolean;
}

/** Any authenticated user. Throws Unauthenticated when absent. */
export async function requireUser(
  ctx: HandlerContext,
  getSession: GetSession = getSessionFromHeaders,
): Promise<RpcUser> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  return {
    id: session.user.id,
    role: session.user.role ?? "user",
    serviceAccount: isServiceAccountEmail(session.user.email ?? ""),
  };
}

/** An authenticated admin. Unauthenticated when absent, PermissionDenied
 * for a non-admin. */
export async function requireAdmin(
  ctx: HandlerContext,
  getSession: GetSession = getSessionFromHeaders,
): Promise<RpcUser> {
  const user = await requireUser(ctx, getSession);
  if (user.role !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
  return user;
}
