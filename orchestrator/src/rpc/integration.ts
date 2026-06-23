/**
 * Native IntegrationService (ADR 0057 C3) — connector catalog CRUD (Plane B).
 *
 * Orchestrator-native (like ProfileService), registered before the passthrough.
 * Connectors live in the orchestrator DB (the `connector` table, C1); the
 * built-ins (`github`/`datadog`) are read-only file seeds surfaced alongside
 * custom ones. Admin-only. `config_json` is the raw connector JSON, validated by
 * `parseConnector` (the admin-trust boundary) on write; a successful write
 * invalidates the merged-registry cache so the next session-create sees it.
 *
 * Injectable deps (getSession, connectors) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import { auth } from "../auth/better-auth.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore, type ConnectorStore } from "../db/connectors.ts";
import { connectorRegistry, parseConnector, invalidateRegistry } from "../connectors/registry.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface IntegrationDeps {
  getSession?: GetSession;
  connectors?: ConnectorStore;
}

async function requireAdmin(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if ((session.user.role ?? "user") !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
}

/** Built-in seed providers (read-only) — a custom connector may not shadow one. */
function builtinProviders(): Set<string> {
  return new Set(connectorRegistry().keys());
}

export function registerIntegration(router: ConnectRouter, deps?: IntegrationDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));
  const connectors: ConnectorStore = deps?.connectors ?? makeConnectorStore(getDb());

  router.service(IntegrationService, {
    async listConnectors(_req, ctx) {
      await requireAdmin(ctx, getSession);
      // Built-in file seeds (read-only) first; then admin-authored DB rows,
      // dropping any that collide with a seed (writes reject collisions, so this
      // is the defensive belt — built-in always wins).
      const seeds = builtinProviders();
      const builtins = [...connectorRegistry().values()].map((c) => ({
        provider: c.provider,
        configJson: JSON.stringify(c),
        builtin: true,
        createdAt: "",
        updatedAt: "",
      }));
      const custom = (await connectors.list())
        .filter((r) => !seeds.has(r.provider))
        .map((r) => ({
          provider: r.provider,
          configJson: JSON.stringify(r.config),
          builtin: false,
          createdAt: r.createdAt.toISOString(),
          updatedAt: r.updatedAt.toISOString(),
        }));
      return { connectors: [...builtins, ...custom] };
    },

    async upsertConnector(req, ctx) {
      await requireAdmin(ctx, getSession);
      let raw: unknown;
      try {
        raw = JSON.parse(req.configJson);
      } catch (e) {
        throw new ConnectError(`connector config is not valid JSON: ${(e as Error).message}`, Code.InvalidArgument);
      }
      // Admin-trust validation boundary (C1 hardening).
      let parsed;
      try {
        parsed = parseConnector(raw, "upload");
      } catch (e) {
        throw new ConnectError((e as Error).message, Code.InvalidArgument);
      }
      if (builtinProviders().has(parsed.provider)) {
        throw new ConnectError(
          `"${parsed.provider}" is a built-in connector and cannot be overridden`,
          Code.InvalidArgument,
        );
      }
      const row = await connectors.upsert(parsed.provider, raw);
      // The next loadRegistry() must see the new connector.
      invalidateRegistry();
      return {
        connector: {
          provider: row.provider,
          configJson: JSON.stringify(row.config),
          builtin: false,
          createdAt: row.createdAt.toISOString(),
          updatedAt: row.updatedAt.toISOString(),
        },
      };
    },

    async deleteConnector(req, ctx) {
      await requireAdmin(ctx, getSession);
      if (builtinProviders().has(req.provider)) {
        throw new ConnectError(
          `"${req.provider}" is a built-in connector and cannot be deleted`,
          Code.InvalidArgument,
        );
      }
      const deleted = await connectors.delete(req.provider);
      if (deleted) invalidateRegistry();
      return { deleted };
    },
  });
}
