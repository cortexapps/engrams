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

function googleConfigFromProto(value: {
  workloadIdentityProvider: string;
  serviceAccountEmail: string;
  endpoints: string[];
} | undefined): GoogleCloudConnectionConfig {
  if (!value) throw new ConnectError("google_cloud config is required", Code.InvalidArgument);
  try {
    return assertGoogleCloudConfig({
      workloadIdentityProvider: value.workloadIdentityProvider,
      serviceAccountEmail: value.serviceAccountEmail,
      endpoints: value.endpoints,
    });
  } catch (error) {
    throw new ConnectError(errorMessage(error), Code.InvalidArgument);
  }
}

function connectionToProto(row: IntegrationConnectionRow) {
  const google = row.provider === "gcp" ? assertGoogleCloudConfig(row.config) : undefined;
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
    googleCloud: google ? {
      workloadIdentityProvider: google.workloadIdentityProvider,
      serviceAccountEmail: google.serviceAccountEmail,
      endpoints: google.endpoints,
    } : undefined,
  };
}

function assertConnectionNames(alias: string, displayName: string): void {
  if (!/^[a-z][a-z0-9-]{1,62}$/.test(alias)) {
    throw new ConnectError("alias must use 2-63 lowercase letters, digits, or hyphens", Code.InvalidArgument);
  }
  if (!displayName.trim()) throw new ConnectError("display_name is required", Code.InvalidArgument);
}

function assertGoogleIssuerAvailable(issuer: string): void {
  let url: URL;
  try {
    url = new URL(issuer);
  } catch {
    throw new ConnectError("Google Cloud WIF requires a valid public issuer URL", Code.FailedPrecondition);
  }
  if (url.protocol !== "https:") {
    throw new ConnectError(
      "Google Cloud WIF requires ORCHESTRATOR_PUBLIC_URL to use public HTTPS",
      Code.FailedPrecondition,
    );
  }
  if (url.username || url.password || !url.hostname) {
    throw new ConnectError(
      "Google Cloud WIF requires a public issuer without URL credentials",
      Code.FailedPrecondition,
    );
  }
}

function googleSetup(row: IntegrationConnectionRow, issuer: string, deploymentId: string): {
  audience: string;
  gcloudScript: string;
  terraform: string;
} {
  const google = assertGoogleCloudConfig(row.config);
  const match = google.workloadIdentityProvider.match(
    /^\/\/iam\.googleapis\.com\/projects\/([0-9]+)\/locations\/global\/workloadIdentityPools\/([a-z0-9-]+)\/providers\/([a-z0-9-]+)$/,
  );
  if (!match) throw new Error("stored Google provider resource is invalid");
  const [, projectNumber, poolId, providerId] = match;
  // Terraform resource names are addresses, not labels: two connections in the
  // same project used to emit `google_iam_workload_identity_pool.engrams`
  // twice, so applying the second setup silently redefined the first. Derive
  // the address from the provider id, which is already unique per connection.
  const tfName = `engrams_${providerId!.replace(/-/g, "_")}`;
  // `engrams_organization` carries the deployment id (the issuer URL already
  // rides in `issuer_uri`, so pinning the URL again added nothing).
  const condition =
    `assertion.engrams_organization == '${deploymentId}' && ` +
    `assertion.engrams_connection == '${row.id}'`;
  const mapping =
    "google.subject=assertion.sub," +
    "attribute.engrams_organization=assertion.engrams_organization," +
    "attribute.engrams_connection=assertion.engrams_connection";
  const principalSet =
    `principalSet://iam.googleapis.com/projects/${projectNumber}/locations/global/` +
    `workloadIdentityPools/${poolId}/attribute.engrams_connection/${row.id}`;
  return {
    audience: google.workloadIdentityProvider,
    // An operator pastes this into a shell. Without a shebang the lines run
    // under whatever shell they happen to use, and without `set -euo pipefail`
    // a failed pool creation is invisible: the next command runs anyway and the
    // script "succeeds" with a half-built pool. Pool and provider creation are
    // describe-then-create so re-running the setup — the normal thing to do
    // after editing endpoints — is not an ALREADY_EXISTS error.
    gcloudScript: [
      `#!/usr/bin/env bash`,
      `set -euo pipefail`,
      ``,
      `gcloud iam workload-identity-pools describe ${poolId} --location=global --project=${projectNumber} >/dev/null 2>&1 ||`,
      `  gcloud iam workload-identity-pools create ${poolId} --location=global --project=${projectNumber}`,
      ``,
      `gcloud iam workload-identity-pools providers describe ${providerId} --location=global --workload-identity-pool=${poolId} --project=${projectNumber} >/dev/null 2>&1 ||`,
      `  gcloud iam workload-identity-pools providers create-oidc ${providerId} --location=global --workload-identity-pool=${poolId} --project=${projectNumber} --issuer-uri=${issuer} --allowed-audiences=${google.workloadIdentityProvider} --attribute-mapping=${mapping} --attribute-condition=\"${condition}\"`,
      ``,
      `gcloud iam service-accounts add-iam-policy-binding ${google.serviceAccountEmail} --project=${projectNumber} --role=roles/iam.workloadIdentityUser --member=${principalSet}`,
    ].join("\n"),
    terraform: [
      `resource "google_iam_workload_identity_pool" "${tfName}" {`,
      `  project                   = "${projectNumber}"`,
      `  workload_identity_pool_id = "${poolId}"`,
      `}`,
      ``,
      `resource "google_iam_workload_identity_pool_provider" "${tfName}" {`,
      `  project                            = "${projectNumber}"`,
      `  workload_identity_pool_id          = google_iam_workload_identity_pool.${tfName}.workload_identity_pool_id`,
      `  workload_identity_pool_provider_id = "${providerId}"`,
      `  attribute_mapping = {`,
      `    "google.subject"                 = "assertion.sub"`,
      `    "attribute.engrams_organization" = "assertion.engrams_organization"`,
      `    "attribute.engrams_connection"   = "assertion.engrams_connection"`,
      `  }`,
      `  attribute_condition = "${condition}"`,
      `  oidc {`,
      `    issuer_uri        = "${issuer}"`,
      `    allowed_audiences = ["${google.workloadIdentityProvider}"]`,
      `  }`,
      `}`,
      ``,
      `resource "google_service_account_iam_member" "${tfName}" {`,
      `  service_account_id = "projects/${projectNumber}/serviceAccounts/${google.serviceAccountEmail}"`,
      `  role               = "roles/iam.workloadIdentityUser"`,
      `  member             = "${principalSet}"`,
      `}`,
    ].join("\n"),
  };
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
  const googleExchange = deps?.googleExchange ?? makeGoogleWifBroker({
    keys: makeIntegrationOidcKeyStore(getDb()),
    issuer,
    now,
  }).exchange;

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
      const providers = await Promise.all(buildProviderCatalog(registry).map(async (e) => {
        const defaultConnection = await connections.getDefault(e.provider);
        if (!defaultConnection) {
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
          capabilities: e.capabilities.map((c) => ({ action: c.action, access: c.access, asset: c.asset ?? "" })),
          defaultConnectionId: defaultConnection.id,
        };
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

    async listConnections(_req, ctx) {
      await requireAdmin(ctx, getSession);
      return { connections: (await connections.list()).map(connectionToProto) };
    },

    async createConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      assertGoogleIssuerAvailable(issuer);
      if (req.provider !== "gcp") {
        throw new ConnectError('provider must be "gcp"', Code.InvalidArgument);
      }
      assertConnectionNames(req.alias, req.displayName);
      const google = googleConfigFromProto(req.googleCloud);
      const row = await connections.create({
        alias: req.alias,
        provider: "gcp",
        displayName: req.displayName.trim(),
        config: { ...google },
      });
      return { connection: connectionToProto(row) };
    },

    async updateConnection(req, ctx) {
      await requireAdmin(ctx, getSession);
      assertGoogleIssuerAvailable(issuer);
      assertConnectionNames(req.alias, req.displayName);
      const current = await connections.get(req.id);
      if (!current) throw new ConnectError("connection not found", Code.NotFound);
      if (current.provider !== "gcp") {
        throw new ConnectError("only Google Cloud connections can be updated here", Code.InvalidArgument);
      }
      const google = googleConfigFromProto(req.googleCloud);
      const row = await connections.update(req.id, {
        alias: req.alias,
        displayName: req.displayName.trim(),
        config: { ...google },
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
      assertGoogleIssuerAvailable(issuer);
      const row = await connections.get(req.id);
      if (!row || row.provider !== "gcp") throw new ConnectError("connection not found", Code.NotFound);
      try {
        const google = assertGoogleCloudConfig(row.config);
        await googleExchange(google, {
          sessionId: `connection-test:${row.id}`,
          organizationId: deploymentId,
          connectionId: row.id,
          userId: "administrator",
          profileSnapshotId: "connection-test",
        });
        await connections.markTested(row.id, now());
        return { ok: true, message: "STS exchange and service account impersonation succeeded" };
      } catch (error) {
        return { ok: false, message: errorMessage(error) };
      }
    },

    async setConnectionEnabled(req, ctx) {
      await requireAdmin(ctx, getSession);
      const current = await connections.get(req.id);
      if (!current) throw new ConnectError("connection not found", Code.NotFound);
      if (req.enabled && current.provider === "gcp") assertGoogleIssuerAvailable(issuer);
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
      assertGoogleIssuerAvailable(issuer);
      const row = await connections.get(req.id);
      if (!row || row.provider !== "gcp") throw new ConnectError("connection not found", Code.NotFound);
      const setup = googleSetup(row, issuer, deploymentId);
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
