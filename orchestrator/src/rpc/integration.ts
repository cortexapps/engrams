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
import { getSessionFromHeaders } from "../auth/session.ts";
import { config } from "../config.ts";
import { errorMessage } from "../log.ts";
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
import {
  makeIntegrationConnectionStore,
  type GoogleCloudConnectionConfig,
  type IntegrationConnectionRow,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { makeIntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import { assertGoogleCloudConfig, makeGoogleWifBroker } from "../integrations/google-wif.ts";
import {
  connectionProvider,
  connectionProviders,
  makeConnectionProviders,
  type ConnectionProvider,
} from "../integrations/providers/index.ts";
import { makeGoogleProvider } from "../integrations/providers/google.ts";
import { googleOidcIssuer } from "../routes/google-oidc.ts";

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
  connections?: IntegrationConnectionStore;
  profiles?: ProfileStore;
  now?: () => Date;
  googleExchange?: ReturnType<typeof makeGoogleWifBroker>["exchange"];
  issuer?: string;
  /** Deployment identity emitted as the `engrams_organization` claim. */
  deploymentId?: string;
  /** Override the whole provider registry (a suite with its own providers). */
  providers?: ReadonlyMap<string, ConnectionProvider>;
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

/**
 * Where each provider's configuration sits on the wire.
 *
 * The proto carries a TYPED message per provider, not an opaque struct, so
 * some mapping from a provider key to a wire field is unavoidable. This table
 * is deliberately the ONLY place it exists: everything downstream — validation,
 * policy compilation, minting, setup — goes through the provider itself.
 */
const PROTO_CONFIG_BY_PROVIDER: Record<string, ProtoConfigCodec> = {
  gcp: {
    read: (req) =>
      req.googleCloud && {
        workloadIdentityProvider: req.googleCloud.workloadIdentityProvider,
        serviceAccountEmail: req.googleCloud.serviceAccountEmail,
        endpoints: req.googleCloud.endpoints,
      },
    write: (config) => {
      const google = assertGoogleCloudConfig(config);
      return {
        googleCloud: {
          workloadIdentityProvider: google.workloadIdentityProvider,
          serviceAccountEmail: google.serviceAccountEmail,
          endpoints: google.endpoints,
        },
      };
    },
  },
};

interface ProtoConfigCodec {
  read(req: { googleCloud?: GoogleCloudProtoConfig }): Record<string, unknown> | undefined;
  write(config: Record<string, unknown>): { googleCloud?: GoogleCloudProtoConfig };
}

interface GoogleCloudProtoConfig {
  workloadIdentityProvider: string;
  serviceAccountEmail: string;
  endpoints: string[];
}

/** Read a request's provider config off the wire and validate it. */
function configFromProto(
  provider: ConnectionProvider,
  req: { googleCloud?: GoogleCloudProtoConfig },
): Record<string, unknown> {
  const raw = PROTO_CONFIG_BY_PROVIDER[provider.key]?.read(req);
  if (!raw) {
    throw new ConnectError(`${provider.key} config is required`, Code.InvalidArgument);
  }
  try {
    return provider.validateConfig(raw);
  } catch (error) {
    throw new ConnectError(errorMessage(error), Code.InvalidArgument);
  }
}

function connectionToProto(row: IntegrationConnectionRow) {
  // A row whose provider is no longer registered still has to serialize — an
  // operator must be able to SEE it in order to delete it.
  const config = PROTO_CONFIG_BY_PROVIDER[row.provider]?.write(row.config) ?? {};
  return {
    id: row.id,
    alias: row.alias,
    provider: row.provider,
    displayName: row.displayName,
    isDefault: row.isDefault,
    enabled: row.enabled,
    testedAt: row.testedAt?.toISOString() ?? "",
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
    ...config,
  };
}

function assertConnectionNames(alias: string, displayName: string): void {
  if (!/^[a-z][a-z0-9-]{1,62}$/.test(alias)) {
    throw new ConnectError("alias must use 2-63 lowercase letters, digits, or hyphens", Code.InvalidArgument);
  }
  if (!displayName.trim()) throw new ConnectError("display_name is required", Code.InvalidArgument);
}


/**
 * The provider a request names, or a clear rejection.
 *
 * These RPCs used to hard-code `"gcp"`. A provider key that is not registered
 * is an unknown provider, not a malformed request, so it reads as NotFound on
 * a lookup and InvalidArgument on a create.
 */
function requireProviderIn(
  providers: ReadonlyMap<string, ConnectionProvider>,
  key: string,
): ConnectionProvider {
  const provider = providers.get(key);
  if (!provider) {
    throw new ConnectError(
      `unknown connection provider "${key}"`,
      Code.InvalidArgument,
    );
  }
  return provider;
}

/** Load a connection row along with its provider, or NotFound. */
async function requireConnectionIn(
  providers: ReadonlyMap<string, ConnectionProvider>,
  connections: IntegrationConnectionStore,
  id: string,
): Promise<{ row: IntegrationConnectionRow; provider: ConnectionProvider }> {
  const row = await connections.get(id);
  const provider = row ? providers.get(row.provider) : undefined;
  if (!row || !provider) throw new ConnectError("connection not found", Code.NotFound);
  return { row, provider };
}

export function registerIntegration(router: ConnectRouter, deps?: IntegrationDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;
  const connectors: ConnectorStore = deps?.connectors ?? makeConnectorStore(getDb());
  const connectorLogos: ConnectorLogoStore = deps?.connectorLogos ?? makeConnectorLogoStore(getDb());
  const orgSecret: OrgSecretAccess = deps?.orgSecret ?? (defaultOrgSecret as unknown as OrgSecretAccess);
  const mint: MintAccess = deps?.mint ?? (defaultMint as unknown as MintAccess);
  const connections = deps?.connections ?? makeIntegrationConnectionStore(getDb());
  const profiles = deps?.profiles ?? makeProfileStore(getDb());
  const now = deps?.now ?? (() => new Date());
  const issuer = deps?.issuer ?? googleOidcIssuer();
  const deploymentId = deps?.deploymentId ?? config.deploymentId;
  // Tests inject a stub exchange; production takes the shared registry.
  const providers = deps?.providers ??
    (deps?.googleExchange
      ? makeConnectionProviders([makeGoogleProvider({ exchange: deps.googleExchange })])
      : connectionProviders());
  const requireProvider = (key: string) => requireProviderIn(providers, key);
  const requireConnection = (id: string) => requireConnectionIn(providers, connections, id);

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
      await connections.ensureDefault(parsed.provider, `${parsed.display.name} (default)`);
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
      const entries = await Promise.all(buildProviderCatalog(registry, providers).map(async (e) => {
        // A NAMED provider has no singleton credential slot — an administrator
        // creates its connections, and `ListConnections` returns them. Only a
        // singleton provider must have a default.
        const defaultConnection = e.connectionModel === "named"
          ? null
          : await connections.getDefault(e.provider);
        if (!defaultConnection && e.connectionModel !== "named") {
          throw new ConnectError(
            `default integration connection for "${e.provider}" is unavailable`,
            Code.Internal,
          );
        }
        return {
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
          capabilities: e.capabilities.map((c) => ({
            action: c.action,
            access: c.access,
            asset: c.asset ?? "",
            label: c.label ?? "",
            host: c.host ?? "",
            endpointRule: c.endpointRule ?? "",
          })),
          defaultConnectionId: defaultConnection?.id ?? "",
          connectionModel: e.connectionModel,
        };
      }));
      return { providers: entries };
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

    async listConnections(_req, ctx) {
      await requireAdmin(ctx, getSession);
      return { connections: (await connections.list()).map(connectionToProto) };
    },

    async createConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      const provider = requireProvider(req.provider);
      provider.assertDeploymentReady?.({ issuer, deploymentId });
      assertConnectionNames(req.alias, req.displayName);
      const config = configFromProto(provider, req);
      const row = await connections.create({
        alias: req.alias,
        provider: provider.key,
        displayName: req.displayName.trim(),
        config,
      });
      return { connection: connectionToProto(row) };
    },

    async updateConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      assertConnectionNames(req.alias, req.displayName);
      const { provider } = await requireConnection(req.id);
      provider.assertDeploymentReady?.({ issuer, deploymentId });
      const config = configFromProto(provider, req);
      const row = await connections.update(req.id, {
        alias: req.alias,
        displayName: req.displayName.trim(),
        config,
      });
      return { connection: connectionToProto(row!) };
    },

    async deleteConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      const connection = await connections.get(req.id);
      if (connection?.isDefault) {
        throw new ConnectError(
          "a provider's default connection cannot be deleted",
          Code.FailedPrecondition,
        );
      }
      const referenced = (await profiles.list({ includeArchived: true })).some(
        (profile) => profile.integrationGrants.some((grant) => grant.connectionId === req.id),
      );
      if (referenced) {
        throw new ConnectError("connection is still granted to a profile", Code.FailedPrecondition);
      }
      return { deleted: await connections.delete(req.id) };
    },

    async testConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      const { row, provider } = await requireConnection(req.id);
      provider.assertDeploymentReady?.({ issuer, deploymentId });
      try {
        // A real mint, discarded. Anything short of one leaves the operator to
        // discover a broken trust policy when a session boots.
        await provider.mint(row, {
          sessionId: `connection-test:${row.id}`,
          organizationId: deploymentId,
          connectionId: row.id,
          userId: "administrator",
          profileSnapshotId: "connection-test",
        });
        await connections.markTested(row.id, now());
        return { ok: true, message: "credential exchange and impersonation succeeded" };
      } catch (error) {
        return { ok: false, message: errorMessage(error) };
      }
    },

    async setConnectionEnabled(req, ctx) {
      await requireAdmin(ctx, getSession);
      const current = await connections.get(req.id);
      if (!current) throw new ConnectError("connection not found", Code.NotFound);
      if (req.enabled) {
        providers.get(current.provider)?.assertDeploymentReady?.({ issuer, deploymentId });
      }
      if (req.enabled && current.testedAt == null) {
        throw new ConnectError(
          "the connection must pass STS and impersonation tests before it can be enabled",
          Code.FailedPrecondition,
        );
      }
      const row = await connections.setEnabled(req.id, req.enabled);
      return { connection: connectionToProto(row!) };
    },

    async getGoogleCloudSetup(req, ctx) {
      await requireAdmin(ctx, getSession);
      const { row, provider } = await requireConnection(req.id);
      provider.assertDeploymentReady?.({ issuer, deploymentId });
      const setup = provider.setupDoc(row, { issuer, deploymentId });
      return {
        issuer,
        audience: setup.audience,
        subjectAttribute: "google.subject=assertion.sub",
        connectionAttribute: "attribute.engrams_connection=assertion.engrams_connection",
        gcloudScript: setup.gcloudScript,
        terraform: setup.terraform,
      };
    },
  });
}
