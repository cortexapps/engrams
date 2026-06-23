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
import type { MintKind } from "../gen/engram/app/v1/mint_pb.ts";
import { auth } from "../auth/better-auth.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore, type ConnectorStore } from "../db/connectors.ts";
import {
  connectorRegistry,
  parseConnector,
  invalidateRegistry,
  loadRegistry,
  buildProviderCatalog,
  connectorStatus,
  type Connector,
} from "../connectors/registry.ts";
import { orgSecret as defaultOrgSecret, mint as defaultMint } from "../control-plane/client.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

/** The slice of the coordinator OrgSecretService this service reads/writes. */
export interface OrgSecretAccess {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
  putSecret(req: { name: string; value: string }): Promise<unknown>;
}
/** The slice of the coordinator MintService this service reads (form metadata). */
export interface MintAccess {
  listMintKinds(req: Record<string, never>): Promise<{ mintKinds: MintKind[] }>;
}

export interface IntegrationDeps {
  getSession?: GetSession;
  connectors?: ConnectorStore;
  orgSecret?: OrgSecretAccess;
  mint?: MintAccess;
}

async function requireAdmin(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if ((session.user.role ?? "user") !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
}

/** Member gate: any authenticated user (the provider catalog is member-readable). */
async function requireUser(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
}

/** Built-in seed providers (read-only) — a custom connector may not shadow one. */
function builtinProviders(): Set<string> {
  return new Set(connectorRegistry().keys());
}

/**
 * One-shot fetch of the inputs `connectorStatus` needs: the org-secret name set
 * and, per mint kind, the org-secret names it requires (`<kind>.<field>` for
 * each required field). Degrades to empty (everything "available") if the
 * coordinator is briefly unreachable, mirroring loadRegistry's degrade policy.
 */
async function fetchStatusInputs(
  orgSecret: OrgSecretAccess,
  mint: MintAccess,
): Promise<{ names: Set<string>; requiredByKind: Map<string, string[]> }> {
  try {
    const [secrets, mintKinds] = await Promise.all([orgSecret.listSecrets({}), mint.listMintKinds({})]);
    const names = new Set(secrets.secrets.map((s) => s.name));
    const requiredByKind = new Map<string, string[]>();
    for (const k of mintKinds.mintKinds) {
      requiredByKind.set(
        k.kind,
        k.fields.filter((f) => f.required).map((f) => `${k.kind}.${f.name}`),
      );
    }
    return { names, requiredByKind };
  } catch (e) {
    console.error(`integration: status inputs unavailable, reporting all connectors available — ${(e as Error).message}`);
    return { names: new Set(), requiredByKind: new Map() };
  }
}

/** Derive a connector's connected/available status from the prefetched inputs. */
function statusOf(c: Connector, names: Set<string>, requiredByKind: Map<string, string[]>): string {
  const required = c.credential.source === "mint" ? (requiredByKind.get(c.credential.mint.kind) ?? []) : [];
  return connectorStatus(c, names, required);
}

export function registerIntegration(router: ConnectRouter, deps?: IntegrationDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));
  const connectors: ConnectorStore = deps?.connectors ?? makeConnectorStore(getDb());
  const orgSecret: OrgSecretAccess = deps?.orgSecret ?? (defaultOrgSecret as unknown as OrgSecretAccess);
  const mint: MintAccess = deps?.mint ?? (defaultMint as unknown as MintAccess);

  router.service(IntegrationService, {
    async listConnectors(_req, ctx) {
      await requireAdmin(ctx, getSession);
      const { names, requiredByKind } = await fetchStatusInputs(orgSecret, mint);
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
        status: statusOf(c, names, requiredByKind),
      }));
      const custom = (await connectors.list())
        .filter((r) => !seeds.has(r.provider))
        .map((r) => {
          // Stored rows are pre-validated (upsert ran parseConnector); re-parse
          // only to derive status — fall back to "available" if it somehow fails.
          let parsed: Connector | null = null;
          try {
            parsed = parseConnector(r.config, r.provider);
          } catch {
            parsed = null;
          }
          return {
            provider: r.provider,
            configJson: JSON.stringify(r.config),
            builtin: false,
            createdAt: r.createdAt.toISOString(),
            updatedAt: r.updatedAt.toISOString(),
            status: parsed ? statusOf(parsed, names, requiredByKind) : "available",
          };
        });
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

    // Member-readable: the derived provider catalog (display + powers + hosts).
    // Drives the Launch receipt + in-session provider icons. No secret material.
    async getIntegrationCatalog(_req, ctx) {
      await requireUser(ctx, getSession);
      const registry = await loadRegistry(connectors);
      const providers = buildProviderCatalog(registry).map((e) => ({
        provider: e.provider,
        display: {
          name: e.display.name,
          category: e.display.category,
          blurb: e.display.blurb,
          icon: { mono: e.display.icon.mono, color: e.display.icon.color, logo: e.display.icon.logo ?? "" },
        },
        credentialSource: e.credentialSource,
        hosts: e.hosts,
        capabilities: e.capabilities.map((c) => ({ action: c.action, access: c.access, asset: c.asset ?? "" })),
      }));
      return { providers };
    },

    // Admin-only: seal a mint kind's fields as org secrets `<kind>.<field>` so
    // the coordinator can mint a token per session. Blank values are skipped
    // (the Replace flow re-sends only changed fields). Unknown kinds/fields are
    // rejected so the secret store isn't polluted.
    async setMintCredential(req, ctx) {
      await requireAdmin(ctx, getSession);
      const kinds = (await mint.listMintKinds({})).mintKinds;
      const kind = kinds.find((k) => k.kind === req.kind);
      if (!kind) throw new ConnectError(`unknown mint kind "${req.kind}"`, Code.InvalidArgument);
      if (req.provider && kind.provider !== req.provider) {
        throw new ConnectError(
          `mint kind "${req.kind}" backs provider "${kind.provider}", not "${req.provider}"`,
          Code.InvalidArgument,
        );
      }
      const fieldNames = new Set(kind.fields.map((f) => f.name));
      const written: string[] = [];
      for (const [field, value] of Object.entries(req.values)) {
        if (!fieldNames.has(field)) {
          throw new ConnectError(`unknown field "${field}" for mint kind "${req.kind}"`, Code.InvalidArgument);
        }
        if (!value) continue; // blank = leave the existing secret unchanged
        const name = `${req.kind}.${field}`;
        await orgSecret.putSecret({ name, value });
        written.push(name);
      }
      return { secretNames: written };
    },
  });
}
