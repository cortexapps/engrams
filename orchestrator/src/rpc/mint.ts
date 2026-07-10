/**
 * Native MintService proxy (ADR 0057 C3).
 *
 * Orchestrator-native (like OrgSecretService), registered on the ConnectRouter
 * before the passthrough so it owns the MintService prefix. Admin-only;
 * delegates to the coordinator's MintService — the read-only mint-kind registry
 * that drives the Plane-A "add a mint integration" form. No secrets cross here
 * (form metadata only).
 *
 * Injectable deps (getSession, mint) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { MintService, type MintKind } from "../gen/engram/app/v1/mint_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { mint as defaultMint } from "../control-plane/client.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

/** The slice of the coordinator MintService client this proxy uses. */
export interface MintClient {
  listMintKinds(req: Record<string, never>): Promise<{ mintKinds: MintKind[] }>;
}

export interface MintDeps {
  getSession?: GetSession;
  mint?: MintClient;
}

async function requireAdmin(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if ((session.user.role ?? "user") !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
}

export function registerMint(router: ConnectRouter, deps?: MintDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;
  const client: MintClient = deps?.mint ?? (defaultMint as unknown as MintClient);

  router.service(MintService, {
    // Admin-only: the mint-kind registry describes how to wire a Plane-A
    // integration; it's settings-surface metadata, not member-facing.
    async listMintKinds(_req, ctx) {
      await requireAdmin(ctx, getSession);
      const resp = await client.listMintKinds({});
      return { mintKinds: resp.mintKinds };
    },
  });
}
