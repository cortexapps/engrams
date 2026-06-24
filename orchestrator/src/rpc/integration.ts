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
import { makeConnectorLogoStore, type ConnectorLogoStore } from "../db/connector-logos.ts";
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

/** Logo upload cap — comfortably fits an SVG (KBs) or a square PNG at icon size. */
const LOGO_MAX_BYTES = 512 * 1024;

/** Same-origin serve path for a provider's uploaded logo (rendered via <img>). */
function logoUrl(provider: string): string {
  return `/api/v1/integrations/${encodeURIComponent(provider)}/logo`;
}

/** Sniff the logo's MIME from its bytes (we accept only SVG or PNG). Null = reject. */
function sniffLogoMediaType(data: Uint8Array): string | null {
  // PNG magic: 89 50 4E 47 0D 0A 1A 0A
  const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (data.length >= 8 && PNG.every((b, i) => data[i] === b)) return "image/png";
  // SVG: XML text whose first element is <svg> (allow a leading <?xml?>/BOM/space).
  const head = new TextDecoder().decode(data.slice(0, 512)).replace(/^﻿/, "").toLowerCase();
  if (head.includes("<svg")) return "image/svg+xml";
  return null;
}

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

/** The slice of the coordinator OrgSecretService this service reads/writes. */
export interface OrgSecretAccess {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
  putSecret(req: { name: string; value: string }): Promise<unknown>;
}
/** Resolved connector test spec sent to the coordinator (mirrors RunConnectorTestRequest). */
export interface RunConnectorTestSpec {
  provider: string;
  host: string;
  source: string;
  /** inject source: every header the connector injects (ADR 0058). */
  injects?: Array<{ header: string; template: string; secretRef: string; draftSecret: string }>;
  kind?: string;
  draftFields?: Record<string, string>;
  /** ADR 0058: probe path (`https://{host}{testPath}`); default `/` coord-side. */
  testPath?: string;
}
/** The slice of the coordinator MintService this service reads/calls. */
export interface MintAccess {
  listMintKinds(req: Record<string, never>): Promise<{ mintKinds: MintKind[] }>;
  runConnectorTest(req: RunConnectorTestSpec): Promise<{ ok: boolean; message: string }>;
}

export interface IntegrationDeps {
  getSession?: GetSession;
  connectors?: ConnectorStore;
  connectorLogos?: ConnectorLogoStore;
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
  const connectorLogos: ConnectorLogoStore = deps?.connectorLogos ?? makeConnectorLogoStore(getDb());
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
      // A removed connector keeps no orphaned logo (custom-only path).
      await connectorLogos.delete(req.provider);
      return { deleted };
    },

    // Member-readable: the derived provider catalog (display + powers + hosts).
    // Drives the Launch receipt + in-session provider icons. No secret material.
    // `icon.logo` is overlaid with the serve URL for providers that have an
    // uploaded logo (the orchestrator-owned overlay); "" otherwise → the web
    // falls back to the deterministic monogram.
    async getIntegrationCatalog(_req, ctx) {
      await requireUser(ctx, getSession);
      const registry = await loadRegistry(connectors);
      const withLogo = new Set(await connectorLogos.listProviders());
      const providers = buildProviderCatalog(registry).map((e) => ({
        provider: e.provider,
        display: {
          name: e.display.name,
          category: e.display.category,
          blurb: e.display.blurb,
          icon: {
            mono: e.display.icon.mono,
            color: e.display.icon.color,
            logo: withLogo.has(e.provider) ? logoUrl(e.provider) : "",
          },
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

    // Admin-only: upload/replace (or, with empty bytes, clear) a connector's
    // logo. Orchestrator-owned overlay; the provider must exist in the merged
    // registry. Bytes are sniffed (SVG or PNG only) and capped at 512 KB.
    async uploadConnectorLogo(req, ctx) {
      await requireAdmin(ctx, getSession);
      const registry = await loadRegistry(connectors);
      if (!registry.has(req.provider)) {
        throw new ConnectError(`unknown connector "${req.provider}"`, Code.InvalidArgument);
      }
      if (req.data.length === 0) {
        await connectorLogos.delete(req.provider);
        return { logoUrl: "" };
      }
      if (req.data.length > LOGO_MAX_BYTES) {
        throw new ConnectError(
          `logo is ${req.data.length} bytes (max ${LOGO_MAX_BYTES}); upload an SVG or a ≤512px square PNG`,
          Code.InvalidArgument,
        );
      }
      const mediaType = sniffLogoMediaType(req.data);
      if (!mediaType) {
        throw new ConnectError("logo must be an SVG or PNG image", Code.InvalidArgument);
      }
      await connectorLogos.put(req.provider, mediaType, Buffer.from(req.data));
      return { logoUrl: logoUrl(req.provider) };
    },

    // Admin-only: test a connector's credential. Build the resolved spec from the
    // registry (host + inject header/template/secretRef OR mint kind, ± draft)
    // and delegate the unseal/mint + benign GET to the coordinator. A failed test
    // is `{ ok: false, message }`, not an RPC error.
    async testConnector(req, ctx) {
      await requireAdmin(ctx, getSession);
      const registry = await loadRegistry(connectors);
      const c = registry.get(req.provider);
      if (!c) throw new ConnectError(`unknown connector "${req.provider}"`, Code.InvalidArgument);
      const host = c.hosts[0];
      if (!host) throw new ConnectError(`connector "${req.provider}" has no host`, Code.InvalidArgument);
      const draft = req.draftValues ?? {};
      // ADR 0058: an honest probe path (default `/` coord-side). Datadog's `/`
      // 307-redirects to a public page so any credential "passes" — it points
      // `test.path` at an endpoint that 401/403s without every injected header.
      const testPath = c.test?.path;
      const spec: RunConnectorTestSpec =
        c.credential.source === "mint"
          ? {
              provider: req.provider,
              host,
              source: "mint",
              kind: c.credential.mint.kind,
              draftFields: draft,
              ...(testPath ? { testPath } : {}),
            }
          : {
              provider: req.provider,
              host,
              source: "inject",
              // ADR 0058: probe EVERY injected header. The web sends the drafted
              // values keyed by org-secret ref (`draft[secretRef]`); a header with
              // no draft falls back to its stored secret coordinator-side.
              injects: c.credential.injects.map((inj) => ({
                header: inj.header,
                template: inj.template ?? "{}",
                secretRef: inj.secretRef,
                draftSecret: draft[inj.secretRef] ?? "",
              })),
              ...(testPath ? { testPath } : {}),
            };
      const { ok, message } = await mint.runConnectorTest(spec);
      return { ok, message };
    },
  });
}
