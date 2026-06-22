/**
 * Native OrgSecretService proxy (ADR 0057 A1).
 *
 * Orchestrator-native (like ProfileService / MountCatalogService), registered on
 * the ConnectRouter before the passthrough so it owns the OrgSecretService
 * prefix. The org secret store is the single value store for profile secrets,
 * integration inject credentials, and the GitHub App mint key. ALL RPCs are
 * admin-only; the orchestrator gates here and delegates to the coordinator's
 * OrgSecretService, which seals the value under the KEK and never returns it
 * (List surfaces metadata only — "set / not set").
 *
 * Injectable deps (getSession, orgSecret) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { OrgSecretService } from "../gen/engram/app/v1/org_secret_pb.ts";
import { auth } from "../auth/better-auth.ts";
import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

/** Org-secret metadata — never the value. */
export interface OrgSecretMetaRow {
  name: string;
  keyId: string;
  createdAt: string;
  updatedAt: string;
}

/** The slice of the coordinator OrgSecretService client this proxy uses. */
export interface OrgSecretClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: OrgSecretMetaRow[] }>;
  putSecret(req: { name: string; value: string }): Promise<{ secret?: OrgSecretMetaRow }>;
  deleteSecret(req: { name: string }): Promise<{ deleted: boolean }>;
}

export interface OrgSecretDeps {
  getSession?: GetSession;
  orgSecret?: OrgSecretClient;
}

async function requireAdmin(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if ((session.user.role ?? "user") !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
}

export function registerOrgSecret(router: ConnectRouter, deps?: OrgSecretDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));
  const client: OrgSecretClient =
    deps?.orgSecret ?? (defaultOrgSecret as unknown as OrgSecretClient);

  router.service(OrgSecretService, {
    // Admin-only: org-secret names describe the deployment's integration /
    // policy surface. Values are never returned by the coordinator.
    async listSecrets(_req, ctx) {
      await requireAdmin(ctx, getSession);
      const resp = await client.listSecrets({});
      return { secrets: resp.secrets };
    },

    async putSecret(req, ctx) {
      await requireAdmin(ctx, getSession);
      const resp = await client.putSecret({ name: req.name, value: req.value });
      return { secret: resp.secret };
    },

    async deleteSecret(req, ctx) {
      await requireAdmin(ctx, getSession);
      const resp = await client.deleteSecret({ name: req.name });
      return { deleted: resp.deleted };
    },
  });
}
