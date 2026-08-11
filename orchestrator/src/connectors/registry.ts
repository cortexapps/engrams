/**
 * ADR 0056 (option B′): orchestrator-owned connector config + the compile step.
 *
 * A *connector* describes one provider as static JSON (`./<provider>.json`): its
 * protocol, where its credential comes from (mint vs inject), which egress hosts
 * it opens, and which operations a capability's `action` unlocks (the `grants`
 * tag). It is the single declarative source for *what a capability unlocks*.
 *
 * The orchestrator is the only tier that parses connectors. It:
 *   1. validates each capability's `provider:action` against the connectors at
 *      profile-save (`grantsCapability`), so the editor offers only granted ops;
 *   2. compiles a profile's bound capabilities → a per-session `IntegrationPolicy`
 *      (`compileIntegrationPolicy`) it ships on `CreateSession`.
 * The coordinator/host never see a connector — they enforce the *compiled* policy
 * (resolving each `secret_ref` host-side). Secret *refs* cross the wire here;
 * secret *values* never leave the coordinator/host.
 *
 * Validation is a hand-written typed loader (matching the codebase's other
 * `assert*Valid` guards) rather than a JSON-Schema dependency: connectors are
 * trusted first-party files, so this is a developer-error guard, not a security
 * boundary. The `protocol` is `http`; GraphQL operations ride `http` too —
 * the *match shape* (`{operation, field}`) selects body-parsed GraphQL gating
 * (ADR 0059), so one connector mixes REST + GraphQL ops sharing powers. `grpc`
 * remains a designed-in shape (ADR 0056 §3) a later phase adds as a proxy parser.
 *
 * This phase compiles only Plane-B *injects* (credential source = inject). Mint
 * connectors (`source: "mint"`, e.g. GitHub) validate + gate capabilities now;
 * their mint scopes are compiled in Phase 5.
 */

import { readdirSync, readFileSync } from "node:fs";

import type { ConnectionProvider } from "../integrations/providers/provider.ts";
import { join } from "node:path";

// ---------------------------------------------------------------------------
// Connector types (the static JSON shape)
// ---------------------------------------------------------------------------

/** One injected auth header. `{}` in `template` is replaced by the resolved
 * secret value host-side (e.g. `"Bearer {}"`, default `"{}"`). `secretRef` names
 * the org secret for a static connector; on an oauth-facet connector it is
 * ABSENT — the value is the connector's OAuth access token, resolved and
 * refreshed from the coordinator's sealed credential store (ADR 0106 addendum). */
export interface InjectHeader {
  header: string;
  secretRef?: string;
  template?: string;
}
/** ADR 0058: a connector may inject ONE OR MORE headers. Most need one (e.g.
 * Datadog's `DD-API-KEY`); some need several (Datadog `pup` needs `DD-API-KEY`
 * AND `DD-APPLICATION-KEY`). Each header resolves its own org secret host-side. */
export interface InjectCredential {
  source: "inject";
  injects: InjectHeader[];
}
export interface MintCredential {
  source: "mint";
  mint: { kind: string };
}
export type Credential = InjectCredential | MintCredential;

/** HTTP request match — the protocol-shaped field for a REST operation (ADR §3). */
export interface HttpMatch {
  method?: string;
  path?: string;
}

/**
 * GraphQL request match — the protocol-shaped field for a GraphQL operation
 * (ADR 0059). A connector stays `protocol: "http"` (GraphQL is HTTP transport);
 * the *match shape* selects body-parsed GraphQL gating, so one connector can mix
 * REST + GraphQL operations sharing the same powers. The egress proxy parses the
 * request body and gates by `(operation, field)` against the connector's
 * {@link Connector.graphqlEndpoint}.
 */
export interface GraphqlMatch {
  operation: "query" | "mutation" | "subscription";
  /** Top-level selection field (aliases resolve to it), e.g. `mergePullRequest`. */
  field: string;
}

/** Discriminate a GraphQL match from an HTTP match (a GraphQL match has `operation`). */
export function isGraphqlMatch(m: HttpMatch | GraphqlMatch): m is GraphqlMatch {
  return (m as GraphqlMatch).operation !== undefined;
}

/**
 * URL-derived fallback for asset fields (GraphQL parity). A GraphQL response
 * echoes only the client's selection set (`gh pr create` selects just
 * `id`+`url`), so fields a REST response would carry can be absent from the
 * body while still encoded in the returned URL. `pattern` is matched against
 * the whole extracted fetchable URL — `{name}` captures one run of characters
 * excluding `/`/`?`/`#`, `{name:int}` additionally requires an integer (and
 * emits a JSON number) — and `fields` renders data fields from the captures,
 * filling ONLY fields the response extractors missed (never overwriting).
 */
export interface AssetUrlFallback {
  pattern: string;
  fields: Record<string, string>;
}

/** Response→asset map (consumed in Phase 4; validated + carried now). A `data`
 * value may be a single extractor path or a FALLBACK CHAIN (`string[]`, tried
 * in order — the proxy takes the first path that resolves). Chains compile to
 * repeated `[field, path]` wire pairs, keeping the wire shape unchanged. */
export interface AssetSpec {
  kind: string;
  surface: "action" | "asset";
  success?: Record<string, unknown>;
  data?: Record<string, string | string[]>;
  fetchable?: Record<string, string>;
  urlFallback?: AssetUrlFallback;
}

export interface Operation {
  /** Capability `action`s that activate this operation. */
  grants: string[];
  /** Either an HTTP match (`method`/`path`) or a GraphQL match (`operation`/`field`). */
  match?: HttpMatch | GraphqlMatch;
  asset?: AssetSpec;
}

/**
 * Per-connector visual identity (the marketplace / profile / session-event icon).
 * `mono` + `color` are always present after {@link parseConnector} (defaulted from
 * the provider), so a connector is never iconless. An uploaded logo is NOT part of
 * the connector config — it's a separate orchestrator overlay (the `connector_logo`
 * store), surfaced as `icon.logo` on the wire by the catalog/listing layer.
 */
export interface ConnectorIcon {
  /** 1–2 char uppercase monogram. */
  mono: string;
  /** Brand tint as a `#RGB` / `#RRGGBB` hex. */
  color: string;
}

/** Display metadata for the marketplace + everywhere a provider renders. Always
 * fully populated after {@link parseConnector} (missing fields default off the
 * provider id). Authored in connector JSON; the slug stays canonical. */
export interface ConnectorDisplay {
  name: string;
  category: string;
  blurb: string;
  icon: ConnectorIcon;
}

/**
 * ADR 0058 §2: how the real credential reaches the upstream request. An **open
 * strategy**, deliberately not a boolean — P1 wires only `inject`, but the type
 * admits the rest so a future request-signing (SigV4) arm slots in without
 * reworking call sites. Orthogonal to {@link Credential} `source`: `inject` leans
 * on the existing host-side header injection whether the value is a static org
 * secret (inject source) or a per-session minted token (mint source).
 *
 *   - `inject`          — host-side header overwrite (P1). The CLI carries a
 *                         harmless dummy; the proxy supplies the real header.
 *   - `substitute`      — broker placeholder swap (already shipped; body/query).
 *   - `in-guest-token`  — mint a short-lived token, materialize it in-guest (P2),
 *                         for CLIs that refuse a dummy.
 *   - `request-signing` — re-sign host-side or deliver a signing key in-guest
 *                         (future; AWS SigV4 et al.). The door §2 keeps open.
 */
export type CredentialDelivery = "inject" | "substitute" | "in-guest-token" | "request-signing";

/** Delivery strategies actually wired in P1. The others are valid types but a
 * connector declaring them is rejected at parse until their phase lands (so we
 * never ship a silently-unauthenticated CLI). */
export const IMPLEMENTED_DELIVERIES: ReadonlySet<CredentialDelivery> = new Set<CredentialDelivery>([
  "inject",
  "substitute",
]);

/** A stub config file agentd writes into the guest so a CLI's local auth gate is
 * satisfied. NEVER a real secret — the real credential is supplied host-side by
 * the egress proxy. */
export interface CliDummyFile {
  /** Guest path; `~`-relative or absolute under the home tree (no `..`). */
  path: string;
  /** Verbatim contents (a harmless placeholder). */
  contents: string;
}

/**
 * ADR 0058 §3: the CLI facet — makes a connector's provider drivable through a
 * native CLI in the shared integrations bundle. Present on built-in *and* custom
 * connectors (validated by {@link parseConnector}).
 */
export interface CliFacet {
  /** PATH command names this connector contributes (basenames of bundle bins). */
  bins: string[];
  /**
   * Where the binary comes from: `bundled` (in the admin-baked integrations
   * bundle), `uploaded` (a novel binary an admin uploaded to the mount_catalog —
   * ADR 0058 uploaded-binary arm; names its catalog bundle in {@link bundle}), or
   * `npx` (runtime-fetched through the egress proxy — a later arm, still rejected).
   */
  binSource: "bundled" | "uploaded" | "npx";
  /** ADR 0058 uploaded-binary arm: for `binSource:"uploaded"`, the mount_catalog
   * bundle name carrying this connector's binary (the upload's registered name);
   * `compileCliIntegrations` unions it into the session's `selected_skills`.
   * Required when `uploaded`, unused otherwise (a bundled CLI's binary lives in the
   * shared integrations bundle). */
  bundle?: string;
  /** Fixed harmless env values agentd sets so the CLI stops gating on local auth
   * state (e.g. `{ "GH_TOKEN": "x-engrams-managed" }`). NEVER a real secret. */
  dummyEnv?: Record<string, string>;
  /** Stub config files agentd writes for the same purpose. */
  dummyFiles?: CliDummyFile[];
  /** How the real credential reaches upstream. Defaults to `inject`. */
  credentialDelivery: CredentialDelivery;
  /** How-to text folded into the per-session discovery skill. */
  doc: string;
}

/** ADR 0058: the "Test connection" probe target. The coordinator GETs
 * `https://{hosts[0]}{path}` with the resolved credential; absent → `/`. A
 * connector whose root doesn't exercise auth (Datadog's `/` 307-redirects to a
 * public page, so any value "passes") points this at an endpoint that 401/403s
 * without a valid credential AND requires every injected header — so a partial or
 * wrong credential fails the test honestly. */
export interface ConnectorTest {
  /** Probe path, must start with `/` (e.g. `/api/v1/dashboard`). */
  path: string;
  /** Probe method; default GET. POST for GraphQL-only endpoints (Linear). */
  method?: "GET" | "POST";
  /** Probe request body (JSON), for POST probes. Bounded. */
  body?: string;
}

/**
 * OAuth 2.0 authorization-code acquisition (the "Add to Slack" button, "Connect
 * Linear"). ADR 0106 addendum: an oauth-facet connector is **OAuth-only** — the
 * obtained tokens (access AND rotating refresh) live in the coordinator's sealed
 * credential store keyed by the connection, never as an org secret, and the
 * platform refreshes them proactively. The app's own credentials (`clientIdRef` /
 * `clientSecretRef`) remain admin-entered org secrets (BYO app). The coordinator
 * (the only tier that can read org secrets) builds the authorize URL + runs the
 * code→token exchange; the orchestrator owns the browser redirect, with the
 * durable coordinator flow row as the CSRF state.
 *
 * `tokenUrl`'s host must be within the connector's `hosts` (the egress-trust
 * boundary — it receives the client secret). `authorizeUrl` may instead sit on
 * `acquisitionHosts`: hosts used only by the admin's browser redirect (e.g.
 * Linear authorizes on `linear.app` while the API lives on `api.linear.app`),
 * NEVER compiled into the session egress policy.
 */
export interface OauthFacet {
  authorizeUrl: string;
  /** Authorize-only hosts (browser redirect surface). Never opened to sessions. */
  acquisitionHosts?: string[];
  tokenUrl: string;
  scopes: string[];
  /** Scope-join delimiter; defaults to `,` (Slack, Linear). */
  scopeDelimiter?: "," | " ";
  /** Bounded provider extras appended to the authorize URL (e.g. Linear
   * `actor: "app"`). Standard OAuth parameter names are rejected. */
  extraAuthorizeParams?: Record<string, string>;
  /** PKCE (S256). Off by default; confidential clients don't need it. */
  pkce?: boolean;
  /** Org secret holding the OAuth app's client id (public, but admin-managed). */
  clientIdRef: string;
  /** Org secret holding the OAuth app's client secret. */
  clientSecretRef: string;
  /** Org secret holding the app's request-signing secret, when the provider verifies
   * inbound webhooks with one (ADR 0060 Slack triggers). Admin-entered like the client
   * creds; absent for providers without an inbound webhook surface. */
  signingSecretRef?: string;
  /** Declarative account-metadata extraction — no per-provider server code. */
  metadata?: OauthMetadataSpec;
}

/** Recognized metadata fields; `accountId` feeds account-switch rejection. */
export type OauthMetadataField = "accountId" | "displayName" | "workspaceId" | "workspaceName";

export interface OauthMetadataSpec {
  /** Field → bounded dot-path into the token response JSON (Slack: `team.id`). */
  fromTokenResponse?: Partial<Record<OauthMetadataField, string>>;
  /** One bounded "who am I" request against a connector host, authenticated with
   * the fresh access token (Linear: GraphQL `viewer`). */
  probe?: OauthMetadataProbe;
}

export interface OauthMetadataProbe {
  method?: "GET" | "POST";
  /** Must be one of the connector's `hosts`; defaults to `hosts[0]`. */
  host?: string;
  path: string;
  body?: string;
  map: Partial<Record<OauthMetadataField, string>>;
}

/**
 * ADR 0115: optional user-scoped credential support. A connector keeps its
 * required org-scoped credential (secretRef inject, connector-subject OAuth,
 * or mint); this facet additionally lets profiles opt individual integrations
 * into the launching user's PERSONAL credential for human sessions.
 *
 * - `oauth: true` reuses the top-level {@link OauthFacet} (same BYO app, same
 *   scopes) with a user subject in the sealed store.
 * - `token` accepts a pasted personal access token; `hint` is the setup line
 *   shown in Settings → Credentials.
 * - `inject` is required for MINT connectors only (the mint engine renders
 *   the org header, so user mode needs its own header spec); forbidden for
 *   inject connectors (the single existing header renders the user value).
 */
/**
 * ADR 0115 amendment: how a USER-subject OAuth flow differs from the org
 * connection's. The org facet's `extraAuthorizeParams` are NEVER inherited —
 * they often pin the ORG identity (Linear's `actor: "app"` makes tokens post
 * as the application); a personal flow must default to acting as the
 * authorizing user. Slack additionally needs `scopesParam: "user_scope"`
 * (else the provider mints a bot token) and `grantPath: "authed_user"` (the
 * user token lives in a nested object of the exchange response).
 */
export interface UserOauthOverrides {
  /** Scopes for the user flow; defaults to the facet's. */
  scopes?: string[];
  /** Authorize query param carrying the joined scopes; default `scope`. */
  scopesParam?: string;
  /** Dot-path to the grant object in the token response; default the root. */
  grantPath?: string;
  /** Authorize extras for the USER flow. Defaults to NONE (org extras are
   * never inherited). Same bounds/reserved-name rules as the facet's. */
  authorizeParams?: Record<string, string>;
  /** Metadata mapping override (`fromTokenResponse` only); defaults to the
   * facet's metadata. */
  metadata?: Pick<OauthMetadataSpec, "fromTokenResponse">;
}

export interface UserCredentialFacet {
  /** `true` reuses the facet as-is (minus its authorize extras); an object
   * overrides the user flow's shape. */
  oauth?: true | UserOauthOverrides;
  token?: { hint: string };
  inject?: { header: string; template: string };
}

export type WebhookVerificationScheme =
  | "github_hmac_sha256"
  | "slack_v0"
  | "generic_hmac_sha256";

export interface WebhookEventSpec {
  key: string;
  displayName: string;
}

/** Declarative payload-path to curated-event-alias mapping. Custom connectors
 * can only provide data in this shape; no module/function reference is loaded
 * from connector JSON. */
export interface WebhookAliasSpec {
  path: string;
  alias: string;
}

export interface WebhookFacet {
  verificationScheme: WebhookVerificationScheme;
  events: WebhookEventSpec[];
  aliases: WebhookAliasSpec[];
}

export interface Connector {
  provider: string;
  /** Only `"http"` is implemented; other values are rejected at load. GraphQL
   * operations ride `http` too (ADR 0059) — the match shape, not this field,
   * selects body-parsed GraphQL gating. */
  protocol: "http";
  credential: Credential;
  hosts: string[];
  /** ADR 0059: the single path GraphQL operations POST to (e.g. `/graphql`).
   * Defaulted to `/graphql` when any operation has a GraphQL match; absent for a
   * pure-REST connector. */
  graphqlEndpoint?: string;
  operations: Operation[];
  /** Always defaulted from `provider` when absent (see {@link parseConnector}). */
  display: ConnectorDisplay;
  /** ADR 0058: optional CLI facet — the provider is drivable through a CLI. */
  cli?: CliFacet;
  /** ADR 0058: optional probe path for "Test connection" (default `/`). */
  test?: ConnectorTest;
  /** Optional OAuth authorization-code acquisition (e.g. Slack "Add to Slack"). */
  oauth?: OauthFacet;
  /** ADR 0115: optional user-scoped credential support (PAT and/or OAuth). */
  userCredential?: UserCredentialFacet;
  /** Optional inbound-webhook taxonomy + declarative curated alias mapping. */
  webhook?: WebhookFacet;
}

// ---------------------------------------------------------------------------
// Compiled policy (the wire artifact — mirrors engram_core::types::IntegrationPolicy)
// ---------------------------------------------------------------------------

/** One Plane-B injection, snake_case to match the Rust serde shape. */
/** Externally-tagged to match the Rust serde shape — the enum also crosses
 * the coord ↔ host bincode wire, which cannot decode a `kind`-tagged form. */
/** The brokered-source union, snake_case + externally tagged to match the Rust
 * serde shape (`CredentialMintSource`, wire v24). */
export type CredentialMintSourceJson =
  | {
      connection: {
        connection_id: string;
        provider: string;
      };
    }
  | {
      /** ADR 0106 addendum: a connector OAuth token resolved from the sealed
       * credential store (subject = the connection id), refreshed there. */
      oauth_connector: {
        connection_id: string;
        provider: string;
      };
    }
  | {
      /** ADR 0115: a USER-scoped connector credential (OAuth or static token)
       * resolved from the sealed store; the launching user's id is stamped at
       * compile time and the coordinator stays principal-agnostic (wire v28).
       * `connection_id` rides for future per-connection user credentials. */
      oauth_user: {
        user_id: string;
        connection_id: string;
        provider: string;
      };
    };

export interface IntegrationInjectJson {
  hosts: string[];
  header_name: string;
  header_template: string;
  /** Static-secret source (inject connectors). Empty for a mint entry. */
  secret_ref: string;
  /** Host-side mint authority. `null` means a static `secret_ref` inject. */
  mint_source: CredentialMintSourceJson | null;
  methods: string[];
  path_globs: string[];
  /** ADR 0059: GraphQL operation type for body-parsed gating ("query" |
   * "mutation" | "subscription"); empty = a REST inject. Paired with `graphql_field`. */
  graphql_operation: string;
  /** ADR 0059: GraphQL top-level field this inject authorizes; empty = REST. */
  graphql_field: string;
}
/** One response-observation spec, snake_case to match the Rust serde shape. */
export interface IntegrationObserveJson {
  hosts: string[];
  methods: string[];
  path_globs: string[];
  provider: string;
  asset_kind: string;
  surface: string;
  success_status_class: string | null;
  /** ADR 0059: GraphQL success rule — emit only when the response has no
   * top-level `errors` (plus HTTP 2xx). Takes precedence over `success_status_class`. */
  success_no_graphql_errors: boolean;
  /** ADR 0059: GraphQL operation type for body-parsed firing; empty = a REST observe. */
  graphql_operation: string;
  /** ADR 0059: GraphQL top-level field this observe fires on; empty = REST. */
  graphql_field: string;
  /** `[field, extractorPath]` pairs (serde `Vec<(String, String)>`). Paths may
   * read the response (`$.resp.*`) or the GraphQL request variables (`$.vars.*`). */
  data: [string, string][];
  fetchable: string | null;
  /** URL-derived fallback fields (serde `Option<ObserveUrlFallback>`) — see
   * {@link AssetUrlFallback}. */
  url_fallback: { pattern: string; fields: [string, string][] } | null;
}
/** ADR 0057: the profile's egress network allow-list (snake_case wire shape). */
export interface IntegrationNetworkJson {
  default: "deny" | "allow";
  allow_hosts: string[];
  allow_host_patterns: string[];
}
/** ADR 0057: one profile-defined secret (snake_case wire shape). Value-free. */
export interface IntegrationSecretJson {
  secret_ref: string;
  env_var: string;
  mode: "literal" | "broker";
  allow_hosts: string[];
  allow_host_patterns: string[];
}
export interface IntegrationPolicyJson {
  injects: IntegrationInjectJson[];
  observes: IntegrationObserveJson[];
  // ADR 0057: the policy is now the full session policy — it also carries the
  // profile's network + secrets (the coordinator sources the egress policy from
  // these). Mirrors engram_core::types::IntegrationPolicy.
  network: IntegrationNetworkJson;
  secrets: IntegrationSecretJson[];
  /** Compatibility services mounted on the session-scoped guest gateway. */
  guest_services: GuestService[];
  /** Host-side byte-stream tunnels. These can contain authority, never tokens. */
  tunnels: SessionTunnelJson[];
}

export interface SessionTunnelJson {
  /** Stable session-local policy name, not a destination. */
  id: string;
  /** Registered host connector kind. */
  connector: string;
  /** Connector-owned JSON that the host validates strictly. */
  config_json: string;
  /** Optional broker authority for connectors that need a short-lived credential. */
  mint_source: {
    connection: { connection_id: string; provider: string };
  } | null;
}

/** Registered host compatibility-service kind. */
export type GuestService = string;

/** Profile-side inputs compiled into the policy's network + secrets (ADR 0057). */
export interface SessionPolicyInputs {
  network?: { default?: string; allowHosts?: string[]; allowHostPatterns?: string[] };
  secrets?: ReadonlyArray<{
    ref: string;
    envVar: string;
    mode?: string;
    allowHosts?: string[];
    allowHostPatterns?: string[];
  }>;
  /** ADR 0115: the launching user's id, supplied ONLY for human principals.
   * A user-scoped grant compiles an `oauth_user` mint source stamped with it;
   * absent (programmatic sessions), user-scoped grants compile the org
   * credential exactly like unscoped ones. */
  userSubjectId?: string;
}

/** Connection-aware authority consumed by policy compilation. Keeping this
 * tuple intact prevents two identities for one provider from being merged. */
export interface IntegrationGrantSelection {
  connectionId: string;
  provider: string;
  operation: string;
  resourceConstraints: readonly string[];
  /** ADR 0115: the profile opted this integration into the launching user's
   * personal credential. Only honored when the compile also supplies a
   * `userSubjectId` (human principals); programmatic sessions compile the
   * org credential regardless. */
  userScoped?: boolean;
}

/** Whether a compiled policy carries anything worth shipping on CreateSession. */
export function policyHasContent(p: IntegrationPolicyJson): boolean {
  return (
    p.injects.length > 0 ||
    p.observes.length > 0 ||
    p.secrets.length > 0 ||
    p.network.allow_hosts.length > 0 ||
    p.network.allow_host_patterns.length > 0 ||
    p.network.default === "allow" ||
    p.guest_services.length > 0 ||
    p.tunnels.length > 0
  );
}

// ---------------------------------------------------------------------------
// Capability parsing (mirrors engram_core::types::Capability::parse)
// ---------------------------------------------------------------------------

export interface ParsedCapability {
  provider: string;
  /** Everything after the first `:` (may itself contain `:`, e.g. `contents:write`). */
  action: string;
  resource: string | null;
}

/** Parse `provider:action[@resource]`; returns null if malformed. */
export function parseCapability(s: string): ParsedCapability | null {
  const at = s.indexOf("@");
  const head = at === -1 ? s : s.slice(0, at);
  const resource = at === -1 ? null : s.slice(at + 1);
  const colon = head.indexOf(":");
  if (colon === -1) return null;
  const provider = head.slice(0, colon);
  const action = head.slice(colon + 1);
  if (!provider || !action || (at !== -1 && !resource)) return null;
  return { provider, action, resource };
}

// ---------------------------------------------------------------------------
// Validation (hand-written typed loader)
// ---------------------------------------------------------------------------

function fail(where: string, msg: string): never {
  throw new Error(`connector ${where}: ${msg}`);
}

function asStringArray(where: string, field: string, v: unknown): string[] {
  if (!Array.isArray(v) || v.length === 0 || !v.every((x) => typeof x === "string" && x.length > 0)) {
    fail(where, `"${field}" must be a non-empty array of non-empty strings`);
  }
  return v as string[];
}

// --- Admin-trust hardening (ADR 0057 C1) -----------------------------------
// parseConnector is no longer a dev-error guard for first-party files: an
// admin-uploaded connector opens egress + injects org secrets, so it is
// validated like a security boundary (same trust level as env_vars / skills
// upload). These bounds + shape checks reject the obviously dangerous or
// malformed before a connector can ever widen a session's reachability.
const MAX_HOSTS = 50;
const MAX_OPERATIONS = 200;
const MAX_GRANTS = 50;
const MAX_INJECTS = 10;
/** RFC 7230 header field-name token (no spaces, colons, or CR/LF). */
const HEADER_NAME_RE = /^[A-Za-z0-9!#$%&'*+.^_`|~-]+$/;
/** Provider id: a lowercase identifier (matches the built-ins). */
const PROVIDER_RE = /^[a-z0-9][a-z0-9_-]*$/;

// --- CLI facet bounds (ADR 0058) -------------------------------------------
const MAX_CLI_BINS = 50;
const MAX_CLI_ENV = 50;
const MAX_CLI_FILES = 20;
const MAX_CLI_FILE_BYTES = 64 * 1024;
const MAX_CLI_DOC_BYTES = 8 * 1024;
/** A PATH command name: a bare basename, no slash/whitespace/control chars. */
const CLI_BIN_RE = /^[A-Za-z0-9._-]+$/;
/** POSIX-ish env var name. */
const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** ADR 0059: a GraphQL field / operation name (matches the egress proxy's parser). */
const GRAPHQL_FIELD_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/**
 * Reject a host that isn't a bare hostname — optionally a single leading-label
 * wildcard (`*.example.com`). No scheme/port/path/whitespace, no bare-TLD or
 * naked `*` wildcard (which would open egress far wider than intended).
 */
function assertHost(where: string, h: string): void {
  if (h.length === 0 || /\s/.test(h)) fail(where, `host "${h}" must be a non-empty hostname with no whitespace`);
  if (/[/:?#@]/.test(h)) fail(where, `host "${h}" must be a bare hostname (no scheme, port, or path)`);
  const wild = h.startsWith("*.");
  const bare = wild ? h.slice(2) : h;
  if (bare.includes("*")) fail(where, `host "${h}" may only wildcard a leading label ("*.example.com")`);
  const labels = bare.split(".");
  if (labels.length < 2) fail(where, `host "${h}" is too broad — need at least "domain.tld"`);
  for (const l of labels) {
    if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(l)) fail(where, `host "${h}" has an invalid label "${l}"`);
  }
}

// --- Display identity (validated + defaulted from the provider id) ---------
const MAX_DISPLAY_NAME = 120;
const MAX_DISPLAY_CATEGORY = 60;
const MAX_DISPLAY_BLURB = 280;
const HEX_COLOR_RE = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;
/** Default tint palette — a stable, contrasty brand-ish color picked by hash so
 * an un-themed connector still gets a distinct, deterministic monogram tile. */
const DEFAULT_ICON_PALETTE = [
  "#1f2328", "#632ca6", "#362d59", "#06ac38",
  "#4a154b", "#0052cc", "#5e6ad2", "#4c4a73",
  "#b8324f", "#c4622d", "#2a6f6f", "#3a5a40",
];

/** FNV-1a 32-bit — a small stable hash for deterministic default tints. */
function hashString(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}
/** Deterministic default tint for a provider (kept in sync with the web mirror). */
export function defaultIconColor(provider: string): string {
  return DEFAULT_ICON_PALETTE[hashString(provider) % DEFAULT_ICON_PALETTE.length]!;
}
/** Default monogram: the first two alphanumerics of the provider, uppercased. */
export function defaultIconMono(provider: string): string {
  const alnum = provider.replace(/[^a-z0-9]/gi, "");
  return (alnum.slice(0, 2) || "?").toUpperCase();
}
/** Default display name: title-cased provider (`pager_duty` → `Pager Duty`). */
export function defaultDisplayName(provider: string): string {
  const words = provider.split(/[-_]+/).filter(Boolean);
  return words.length === 0 ? provider : words.map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
}

/** The fully-defaulted identity for a provider with no authored `display`. */
function defaultDisplay(provider: string): ConnectorDisplay {
  return {
    name: defaultDisplayName(provider),
    category: "Other",
    blurb: "",
    icon: { mono: defaultIconMono(provider), color: defaultIconColor(provider) },
  };
}

/** Validate the optional `display` block, filling any missing field from the
 * provider id so the parsed connector always carries a complete identity. */
function parseDisplay(where: string, raw: unknown, provider: string): ConnectorDisplay {
  const base = defaultDisplay(provider);
  if (raw === undefined) return base;
  if (typeof raw !== "object" || raw === null) fail(where, '"display" must be an object');
  const d = raw as Record<string, unknown>;

  let name = base.name;
  if (d.name !== undefined) {
    if (typeof d.name !== "string") fail(where, '"display.name" must be a string');
    const t = d.name.trim();
    if (t.length === 0 || t.length > MAX_DISPLAY_NAME) fail(where, `"display.name" must be 1..${MAX_DISPLAY_NAME} characters`);
    name = t;
  }
  let category = base.category;
  if (d.category !== undefined) {
    if (typeof d.category !== "string") fail(where, '"display.category" must be a string');
    const t = d.category.trim();
    if (t.length === 0 || t.length > MAX_DISPLAY_CATEGORY) fail(where, `"display.category" must be 1..${MAX_DISPLAY_CATEGORY} characters`);
    category = t;
  }
  let blurb = base.blurb;
  if (d.blurb !== undefined) {
    if (typeof d.blurb !== "string") fail(where, '"display.blurb" must be a string');
    if (d.blurb.length > MAX_DISPLAY_BLURB) fail(where, `"display.blurb" must be at most ${MAX_DISPLAY_BLURB} characters`);
    blurb = d.blurb;
  }
  let icon: ConnectorIcon = base.icon;
  if (d.icon !== undefined) {
    if (typeof d.icon !== "object" || d.icon === null) fail(where, '"display.icon" must be an object');
    const ic = d.icon as Record<string, unknown>;
    const next: ConnectorIcon = { mono: base.icon.mono, color: base.icon.color };
    if (ic.mono !== undefined) {
      if (typeof ic.mono !== "string") fail(where, '"display.icon.mono" must be a string');
      const m = ic.mono.trim().toUpperCase();
      if (m.length < 1 || m.length > 2 || /\s/.test(m)) fail(where, '"display.icon.mono" must be a 1–2 character monogram');
      next.mono = m;
    }
    if (ic.color !== undefined) {
      if (typeof ic.color !== "string" || !HEX_COLOR_RE.test(ic.color)) fail(where, '"display.icon.color" must be a #RGB or #RRGGBB hex color');
      next.color = ic.color;
    }
    icon = next;
  }
  return { name, category, blurb, icon };
}

/**
 * Validate the optional `cli` facet (ADR 0058). Admin-trust boundary: a CLI facet
 * adds PATH binaries + env + stub files to a session, so it is bounds- and
 * shape-checked like `hosts`/`operations`. `dummyEnv`/`dummyFiles` are placeholders
 * the egress proxy makes real host-side — they must never carry a real secret, but
 * that is a soundness property of the author, not something we can detect here; we
 * only enforce shape + safety (no `..` traversal, no CRLF, size caps).
 */
function parseCli(where: string, raw: unknown): CliFacet {
  if (typeof raw !== "object" || raw === null) fail(where, '"cli" must be an object');
  const o = raw as Record<string, unknown>;

  const bins = asStringArray(where, "cli.bins", o.bins);
  if (bins.length > MAX_CLI_BINS) fail(where, `"cli.bins" has ${bins.length} entries (max ${MAX_CLI_BINS})`);
  for (const b of bins) {
    if (!CLI_BIN_RE.test(b)) fail(where, `"cli.bins" entry "${b}" must be a bare command name ([A-Za-z0-9._-]+)`);
  }

  let binSource: CliFacet["binSource"] = "bundled";
  if (o.binSource !== undefined) {
    if (o.binSource !== "bundled" && o.binSource !== "uploaded" && o.binSource !== "npx") {
      fail(where, `"cli.binSource" must be "bundled" | "uploaded" | "npx" (got ${JSON.stringify(o.binSource)})`);
    }
    binSource = o.binSource;
    // `uploaded` rides the ADR 0055 P2 catalog (UB1: an admin-uploaded binary
    // bundle); `npx` (runtime fetch) is designed-for but still not wired.
    if (binSource === "npx") {
      fail(where, `"cli.binSource" "npx" is not yet implemented (ADR 0058: runtime npx is a later arm)`);
    }
  }

  let bundle: string | undefined;
  if (o.bundle !== undefined) {
    if (typeof o.bundle !== "string" || !/^[a-z0-9_-]{1,64}$/.test(o.bundle)) {
      fail(where, '"cli.bundle" must be a mount_catalog bundle name (lowercase alphanumerics, dash, underscore; 1–64 chars)');
    }
    bundle = o.bundle;
  }
  if (binSource === "uploaded" && !bundle) {
    fail(where, '"cli.bundle" is required when "cli.binSource" is "uploaded" (the mount_catalog bundle carrying the binary)');
  }
  if (binSource !== "uploaded" && bundle !== undefined) {
    fail(where, '"cli.bundle" is only valid when "cli.binSource" is "uploaded"');
  }

  let dummyEnv: Record<string, string> | undefined;
  if (o.dummyEnv !== undefined) {
    if (typeof o.dummyEnv !== "object" || o.dummyEnv === null || Array.isArray(o.dummyEnv)) {
      fail(where, '"cli.dummyEnv" must be an object of string→string');
    }
    const ents = Object.entries(o.dummyEnv as Record<string, unknown>);
    if (ents.length > MAX_CLI_ENV) fail(where, `"cli.dummyEnv" has ${ents.length} keys (max ${MAX_CLI_ENV})`);
    const out: Record<string, string> = {};
    for (const [k, v] of ents) {
      if (!ENV_NAME_RE.test(k)) fail(where, `"cli.dummyEnv" key "${k}" is not a valid env var name`);
      if (typeof v !== "string") fail(where, `"cli.dummyEnv.${k}" must be a string`);
      if (/[\r\n\0]/.test(v)) fail(where, `"cli.dummyEnv.${k}" must not contain newlines or NUL`);
      out[k] = v;
    }
    dummyEnv = out;
  }

  let dummyFiles: CliDummyFile[] | undefined;
  if (o.dummyFiles !== undefined) {
    if (!Array.isArray(o.dummyFiles)) fail(where, '"cli.dummyFiles" must be an array');
    if (o.dummyFiles.length > MAX_CLI_FILES) fail(where, `"cli.dummyFiles" has ${o.dummyFiles.length} entries (max ${MAX_CLI_FILES})`);
    dummyFiles = o.dummyFiles.map((rawF, i): CliDummyFile => {
      const fWhere = `${where} cli.dummyFiles[${i}]`;
      if (typeof rawF !== "object" || rawF === null) fail(fWhere, "must be an object");
      const f = rawF as Record<string, unknown>;
      if (typeof f.path !== "string" || !f.path) fail(fWhere, '"path" must be a non-empty string');
      if (f.path.includes("..") || /[\r\n\0]/.test(f.path)) fail(fWhere, '"path" must not contain ".." or control chars');
      if (!f.path.startsWith("~/") && !f.path.startsWith("/")) fail(fWhere, '"path" must be absolute or "~/"-relative');
      if (typeof f.contents !== "string") fail(fWhere, '"contents" must be a string');
      if (Buffer.byteLength(f.contents, "utf8") > MAX_CLI_FILE_BYTES) fail(fWhere, `"contents" exceeds ${MAX_CLI_FILE_BYTES} bytes`);
      return { path: f.path, contents: f.contents };
    });
  }

  let credentialDelivery: CredentialDelivery = "inject";
  if (o.credentialDelivery !== undefined) {
    const d = o.credentialDelivery;
    if (d !== "inject" && d !== "substitute" && d !== "in-guest-token" && d !== "request-signing") {
      fail(where, `"cli.credentialDelivery" must be one of inject|substitute|in-guest-token|request-signing (got ${JSON.stringify(d)})`);
    }
    if (!IMPLEMENTED_DELIVERIES.has(d)) {
      fail(where, `"cli.credentialDelivery" "${d}" is a designed-for strategy not yet wired (ADR 0058 P1 implements "inject"); using it would ship an unauthenticated CLI`);
    }
    credentialDelivery = d;
  }

  if (typeof o.doc !== "string" || !o.doc.trim()) fail(where, '"cli.doc" must be a non-empty string');
  if (Buffer.byteLength(o.doc, "utf8") > MAX_CLI_DOC_BYTES) fail(where, `"cli.doc" exceeds ${MAX_CLI_DOC_BYTES} bytes`);

  return {
    bins,
    binSource,
    ...(bundle ? { bundle } : {}),
    ...(dummyEnv ? { dummyEnv } : {}),
    ...(dummyFiles ? { dummyFiles } : {}),
    credentialDelivery,
    doc: o.doc,
  };
}

const MAX_OAUTH_SCOPES = 50;
const MAX_OAUTH_EXTRA_PARAMS = 16;
const MAX_OAUTH_PARAM_LENGTH = 200;
const MAX_OAUTH_PROBE_BODY = 4096;
const MAX_OAUTH_ACQUISITION_HOSTS = 4;
const MAX_OAUTH_METADATA_PATH_SEGMENTS = 8;
/** An org-secret ref: non-empty, no whitespace (matches the inject secretRef rule). */
const SECRET_REF_RE = /^\S+$/;
/** Parameters the flow machinery owns; a facet may not override them. */
const RESERVED_OAUTH_PARAMS = new Set([
  "client_id",
  "client_secret",
  "redirect_uri",
  "state",
  "scope",
  "response_type",
  "grant_type",
  "code",
  "code_challenge",
  "code_challenge_method",
  "code_verifier",
]);
const OAUTH_METADATA_FIELDS: ReadonlySet<string> = new Set([
  "accountId",
  "displayName",
  "workspaceId",
  "workspaceName",
]);
const OAUTH_METADATA_SEGMENT_RE = /^[A-Za-z0-9_]{1,64}$/;

/** Validate the OAuth acquisition facet (admin-trust boundary). `tokenUrl` must
 * resolve to one of the connector's `hosts` (it receives the client secret, so an
 * admin-authored connector can't ship it elsewhere); `authorizeUrl` may instead
 * sit on the facet's own `acquisitionHosts` — a browser-redirect surface that is
 * never compiled into the session egress policy. */
function parseOauth(where: string, raw: unknown, hosts: string[]): OauthFacet {
  if (typeof raw !== "object" || raw === null) fail(where, '"oauth" must be an object');
  const o = raw as Record<string, unknown>;

  let acquisitionHosts: string[] | undefined;
  if (o.acquisitionHosts !== undefined) {
    acquisitionHosts = asStringArray(`${where} oauth`, "acquisitionHosts", o.acquisitionHosts);
    if (acquisitionHosts.length > MAX_OAUTH_ACQUISITION_HOSTS) {
      fail(where, `"oauth.acquisitionHosts" has ${acquisitionHosts.length} entries (max ${MAX_OAUTH_ACQUISITION_HOSTS})`);
    }
    for (const h of acquisitionHosts) assertHost(where, h);
  }

  const httpsUrlOn = (field: string, v: unknown, allowed: string[]): string => {
    if (typeof v !== "string" || !v) fail(where, `"oauth.${field}" must be a non-empty string`);
    if (/[\s\r\n]/.test(v as string)) fail(where, `"oauth.${field}" must not contain whitespace`);
    let url: URL;
    try {
      url = new URL(v as string);
    } catch {
      return fail(where, `"oauth.${field}" must be a valid URL`);
    }
    if (url.protocol !== "https:") fail(where, `"oauth.${field}" must be an https URL`);
    if (!allowed.includes(url.host)) {
      fail(where, `"oauth.${field}" host "${url.host}" must be one of (${allowed.join(", ")})`);
    }
    return v as string;
  };

  const authorizeUrl = httpsUrlOn("authorizeUrl", o.authorizeUrl, [
    ...hosts,
    ...(acquisitionHosts ?? []),
  ]);
  // Strict: the token exchange carries the client secret.
  const tokenUrl = httpsUrlOn("tokenUrl", o.tokenUrl, hosts);

  const scopes = asStringArray(`${where} oauth`, "scopes", o.scopes);
  if (scopes.length > MAX_OAUTH_SCOPES) fail(where, `"oauth.scopes" has ${scopes.length} entries (max ${MAX_OAUTH_SCOPES})`);
  for (const s of scopes) {
    if (/[\s\r\n]/.test(s)) fail(where, `"oauth.scopes" entry "${s}" must not contain whitespace`);
  }

  let scopeDelimiter: "," | " " | undefined;
  if (o.scopeDelimiter !== undefined) {
    if (o.scopeDelimiter !== "," && o.scopeDelimiter !== " ") {
      fail(where, '"oauth.scopeDelimiter" must be "," or " "');
    }
    scopeDelimiter = o.scopeDelimiter;
  }

  let extraAuthorizeParams: Record<string, string> | undefined;
  if (o.extraAuthorizeParams !== undefined) {
    if (
      typeof o.extraAuthorizeParams !== "object" ||
      o.extraAuthorizeParams === null ||
      Array.isArray(o.extraAuthorizeParams)
    ) {
      fail(where, '"oauth.extraAuthorizeParams" must be an object of param → value');
    }
    const entries = Object.entries(o.extraAuthorizeParams as Record<string, unknown>);
    if (entries.length > MAX_OAUTH_EXTRA_PARAMS) {
      fail(where, `"oauth.extraAuthorizeParams" has ${entries.length} entries (max ${MAX_OAUTH_EXTRA_PARAMS})`);
    }
    extraAuthorizeParams = {};
    for (const [k, v] of entries) {
      if (RESERVED_OAUTH_PARAMS.has(k)) fail(where, `"oauth.extraAuthorizeParams" key "${k}" is reserved`);
      if (
        !k ||
        k.length > MAX_OAUTH_PARAM_LENGTH ||
        typeof v !== "string" ||
        v.length > MAX_OAUTH_PARAM_LENGTH ||
        /[\x00-\x1f\x7f]/.test(k) ||
        /[\x00-\x1f\x7f]/.test(v)
      ) {
        fail(where, '"oauth.extraAuthorizeParams" entries must be short, control-free strings');
      }
      extraAuthorizeParams[k] = v;
    }
  }

  if (o.pkce !== undefined && typeof o.pkce !== "boolean") fail(where, '"oauth.pkce" must be a boolean');

  const secretRef = (field: string, v: unknown): string => {
    if (typeof v !== "string" || !SECRET_REF_RE.test(v)) {
      fail(where, `"oauth.${field}" must be a non-empty string with no whitespace`);
    }
    return v as string;
  };
  if (o.tokenSecretRef !== undefined || o.tokenResponsePath !== undefined) {
    fail(
      where,
      '"oauth.tokenSecretRef"/"oauth.tokenResponsePath" are retired: obtained tokens live in the sealed credential store, not org secrets',
    );
  }

  const metadataMap = (
    field: string,
    v: unknown,
  ): Partial<Record<OauthMetadataField, string>> => {
    if (typeof v !== "object" || v === null || Array.isArray(v)) {
      fail(where, `"oauth.${field}" must be an object of field → dot-path`);
    }
    const out: Partial<Record<OauthMetadataField, string>> = {};
    for (const [k, path] of Object.entries(v as Record<string, unknown>)) {
      if (!OAUTH_METADATA_FIELDS.has(k)) {
        fail(where, `"oauth.${field}" key "${k}" is not a metadata field (${[...OAUTH_METADATA_FIELDS].join(", ")})`);
      }
      if (typeof path !== "string" || !path) fail(where, `"oauth.${field}.${k}" must be a non-empty dot-path`);
      const segments = path.split(".");
      if (segments.length > MAX_OAUTH_METADATA_PATH_SEGMENTS) {
        fail(where, `"oauth.${field}.${k}" has too many path segments (max ${MAX_OAUTH_METADATA_PATH_SEGMENTS})`);
      }
      for (const seg of segments) {
        if (!OAUTH_METADATA_SEGMENT_RE.test(seg) || UNSAFE_OBJECT_PATH_SEGMENTS.has(seg)) {
          fail(where, `"oauth.${field}.${k}" segment "${seg}" is not allowed`);
        }
      }
      out[k as OauthMetadataField] = path;
    }
    return out;
  };

  let metadata: OauthMetadataSpec | undefined;
  if (o.metadata !== undefined) {
    if (typeof o.metadata !== "object" || o.metadata === null) fail(where, '"oauth.metadata" must be an object');
    const m = o.metadata as Record<string, unknown>;
    metadata = {};
    if (m.fromTokenResponse !== undefined) {
      metadata.fromTokenResponse = metadataMap("metadata.fromTokenResponse", m.fromTokenResponse);
    }
    if (m.probe !== undefined) {
      if (typeof m.probe !== "object" || m.probe === null) fail(where, '"oauth.metadata.probe" must be an object');
      const p = m.probe as Record<string, unknown>;
      if (p.method !== undefined && p.method !== "GET" && p.method !== "POST") {
        fail(where, '"oauth.metadata.probe.method" must be "GET" or "POST"');
      }
      if (p.host !== undefined) {
        if (typeof p.host !== "string" || !hosts.includes(p.host)) {
          fail(where, `"oauth.metadata.probe.host" must be one of the connector's hosts (${hosts.join(", ")})`);
        }
      }
      if (typeof p.path !== "string" || !p.path.startsWith("/") || p.path.length > MAX_OAUTH_PARAM_LENGTH) {
        fail(where, '"oauth.metadata.probe.path" must be a short absolute path');
      }
      if (p.body !== undefined && (typeof p.body !== "string" || p.body.length > MAX_OAUTH_PROBE_BODY)) {
        fail(where, '"oauth.metadata.probe.body" must be a bounded string');
      }
      const map = metadataMap("metadata.probe.map", p.map);
      if (Object.keys(map).length === 0) fail(where, '"oauth.metadata.probe.map" must map at least one field');
      metadata.probe = {
        ...(p.method !== undefined ? { method: p.method as "GET" | "POST" } : {}),
        ...(p.host !== undefined ? { host: p.host as string } : {}),
        path: p.path,
        ...(p.body !== undefined ? { body: p.body as string } : {}),
        map,
      };
    }
    if (metadata.fromTokenResponse === undefined && metadata.probe === undefined) {
      fail(where, '"oauth.metadata" must declare fromTokenResponse and/or probe');
    }
  }

  return {
    authorizeUrl,
    ...(acquisitionHosts !== undefined ? { acquisitionHosts } : {}),
    tokenUrl,
    scopes,
    ...(scopeDelimiter !== undefined ? { scopeDelimiter } : {}),
    ...(extraAuthorizeParams !== undefined ? { extraAuthorizeParams } : {}),
    ...(o.pkce !== undefined ? { pkce: o.pkce as boolean } : {}),
    clientIdRef: secretRef("clientIdRef", o.clientIdRef),
    clientSecretRef: secretRef("clientSecretRef", o.clientSecretRef),
    ...(o.signingSecretRef !== undefined
      ? { signingSecretRef: secretRef("signingSecretRef", o.signingSecretRef) }
      : {}),
    ...(metadata !== undefined ? { metadata } : {}),
  };
}

const MAX_WEBHOOK_EVENTS = 200;
const MAX_WEBHOOK_ALIASES = 200;
const MAX_WEBHOOK_EVENT_KEY_LENGTH = 160;
const MAX_WEBHOOK_PATH_LENGTH = 512;
const MAX_WEBHOOK_ALIAS_LENGTH = 160;
const WEBHOOK_EVENT_KEY_RE = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/;
const WEBHOOK_ALIAS_RE = /^[a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)*$/;
const WEBHOOK_PATH_RE = /^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$/;
const UNSAFE_OBJECT_PATH_SEGMENTS = new Set(["__proto__", "constructor", "prototype"]);
const WEBHOOK_SCHEMES: ReadonlySet<WebhookVerificationScheme> = new Set([
  "github_hmac_sha256",
  "slack_v0",
  "generic_hmac_sha256",
]);

/** Parse the optional connector webhook facet at the same allowlist boundary as
 * every other connector field. The result is bounded and declarative only. */
export function parseWebhookFacet(where: string, raw: unknown): WebhookFacet {
  if (typeof raw !== "object" || raw === null) fail(where, '"webhook" must be an object');
  const o = raw as Record<string, unknown>;
  if (
    typeof o.verificationScheme !== "string" ||
    !WEBHOOK_SCHEMES.has(o.verificationScheme as WebhookVerificationScheme)
  ) {
    fail(
      where,
      '"webhook.verificationScheme" must be "github_hmac_sha256", "slack_v0", or "generic_hmac_sha256"',
    );
  }

  if (!Array.isArray(o.events)) fail(where, '"webhook.events" must be an array');
  if (o.events.length > MAX_WEBHOOK_EVENTS) {
    fail(where, `"webhook.events" has ${o.events.length} entries (max ${MAX_WEBHOOK_EVENTS})`);
  }
  const eventKeys = new Set<string>();
  const events = o.events.map((rawEvent, i): WebhookEventSpec => {
    const eventWhere = `${where} webhook.events[${i}]`;
    if (typeof rawEvent !== "object" || rawEvent === null) fail(eventWhere, "must be an object");
    const event = rawEvent as Record<string, unknown>;
    if (
      typeof event.key !== "string" ||
      event.key.length > MAX_WEBHOOK_EVENT_KEY_LENGTH ||
      !WEBHOOK_EVENT_KEY_RE.test(event.key)
    ) {
      fail(eventWhere, '"key" must be a lowercase dot-delimited identifier');
    }
    if (eventKeys.has(event.key)) fail(eventWhere, `duplicate event key "${event.key}"`);
    eventKeys.add(event.key);
    if (
      typeof event.displayName !== "string" ||
      !event.displayName.trim() ||
      event.displayName.length > 120
    ) {
      fail(eventWhere, '"displayName" must be a non-empty string of at most 120 characters');
    }
    return { key: event.key, displayName: event.displayName };
  });

  if (!Array.isArray(o.aliases)) fail(where, '"webhook.aliases" must be an array');
  if (o.aliases.length > MAX_WEBHOOK_ALIASES) {
    fail(where, `"webhook.aliases" has ${o.aliases.length} entries (max ${MAX_WEBHOOK_ALIASES})`);
  }
  const aliasNames = new Set<string>();
  const aliases = o.aliases.map((rawAlias, i): WebhookAliasSpec => {
    const aliasWhere = `${where} webhook.aliases[${i}]`;
    if (typeof rawAlias !== "object" || rawAlias === null) fail(aliasWhere, "must be an object");
    const alias = rawAlias as Record<string, unknown>;
    if (
      typeof alias.path !== "string" ||
      alias.path.length > MAX_WEBHOOK_PATH_LENGTH ||
      !WEBHOOK_PATH_RE.test(alias.path) ||
      alias.path.split(".").some((segment) => UNSAFE_OBJECT_PATH_SEGMENTS.has(segment))
    ) {
      fail(aliasWhere, '"path" must be a dot-delimited payload path');
    }
    if (
      typeof alias.alias !== "string" ||
      alias.alias.length > MAX_WEBHOOK_ALIAS_LENGTH ||
      !WEBHOOK_ALIAS_RE.test(alias.alias) ||
      alias.alias === "raw" ||
      alias.alias.startsWith("raw.") ||
      alias.alias.split(".").some((segment) => UNSAFE_OBJECT_PATH_SEGMENTS.has(segment))
    ) {
      fail(aliasWhere, '"alias" must be a lowercase dot-delimited name outside event.raw');
    }
    if (aliasNames.has(alias.alias)) fail(aliasWhere, `duplicate alias "${alias.alias}"`);
    for (const existing of aliasNames) {
      if (existing.startsWith(`${alias.alias}.`) || alias.alias.startsWith(`${existing}.`)) {
        fail(aliasWhere, `alias "${alias.alias}" conflicts with "${existing}"`);
      }
    }
    aliasNames.add(alias.alias);
    return { path: alias.path, alias: alias.alias };
  });

  return {
    verificationScheme: o.verificationScheme as WebhookVerificationScheme,
    events,
    aliases,
  };
}

/** Validate + narrow one raw connector object. Throws Error on any malformation. */
export function parseConnector(raw: unknown, where: string): Connector {
  if (typeof raw !== "object" || raw === null) fail(where, "must be a JSON object");
  const o = raw as Record<string, unknown>;

  if (typeof o.provider !== "string" || !o.provider) fail(where, '"provider" must be a non-empty string');
  if (!PROVIDER_RE.test(o.provider)) fail(where, `"provider" "${o.provider}" must be a lowercase identifier ([a-z0-9][a-z0-9_-]*)`);
  if (o.protocol !== "http") fail(where, `"protocol" must be "http" (got ${JSON.stringify(o.protocol)}); GraphQL rides http via a "{operation, field}" match (ADR 0059), grpc is not yet implemented`);

  const cred = o.credential as Record<string, unknown> | undefined;
  if (typeof cred !== "object" || cred === null) fail(where, '"credential" must be an object');
  let credential: Credential;
  if (cred.source === "inject") {
    if (!Array.isArray(cred.injects) || cred.injects.length === 0) {
      fail(where, '"credential.injects" must be a non-empty array of {header, secretRef, template?}');
    }
    if (cred.injects.length > MAX_INJECTS) fail(where, `"credential.injects" has ${cred.injects.length} entries (max ${MAX_INJECTS})`);
    const injects: InjectHeader[] = cred.injects.map((raw, i): InjectHeader => {
      const iw = `${where} credential.injects[${i}]`;
      if (typeof raw !== "object" || raw === null) fail(iw, "must be an object");
      const inj = raw as Record<string, unknown>;
      if (typeof inj.header !== "string" || !inj.header) fail(iw, '"header" must be a non-empty string');
      if (!HEADER_NAME_RE.test(inj.header)) fail(iw, `"header" "${inj.header}" is not a valid HTTP header name`);
      // Optional here; the oauth/static cross-check runs after the oauth
      // facet parses (an oauth-facet connector must NOT carry one).
      if (inj.secretRef !== undefined) {
        if (typeof inj.secretRef !== "string" || !inj.secretRef) fail(iw, '"secretRef" must be a non-empty string');
        if (/\s/.test(inj.secretRef)) fail(iw, '"secretRef" must not contain whitespace');
      }
      if (inj.template !== undefined) {
        if (typeof inj.template !== "string") fail(iw, '"template" must be a string');
        if (/[\r\n]/.test(inj.template)) fail(iw, '"template" must not contain newlines');
        if (!inj.template.includes("{}")) fail(iw, '"template" must contain the "{}" value placeholder');
      }
      return {
        header: inj.header,
        ...(typeof inj.secretRef === "string" ? { secretRef: inj.secretRef } : {}),
        ...(typeof inj.template === "string" ? { template: inj.template } : {}),
      };
    });
    credential = { source: "inject", injects };
  } else if (cred.source === "mint") {
    const mint = cred.mint as Record<string, unknown> | undefined;
    if (typeof mint !== "object" || mint === null) fail(where, '"credential.mint" must be an object');
    if (typeof mint.kind !== "string" || !mint.kind) fail(where, '"credential.mint.kind" must be a non-empty string');
    if (/\s/.test(mint.kind)) fail(where, '"credential.mint.kind" must not contain whitespace');
    credential = { source: "mint", mint: { kind: mint.kind } };
  } else {
    fail(where, `"credential.source" must be "inject" or "mint" (got ${JSON.stringify(cred.source)})`);
  }

  const hosts = asStringArray(where, "hosts", o.hosts);
  if (hosts.length > MAX_HOSTS) fail(where, `"hosts" has ${hosts.length} entries (max ${MAX_HOSTS})`);
  for (const h of hosts) assertHost(where, h);

  if (!Array.isArray(o.operations)) fail(where, '"operations" must be an array');
  if (o.operations.length > MAX_OPERATIONS) fail(where, `"operations" has ${o.operations.length} entries (max ${MAX_OPERATIONS})`);
  const operations: Operation[] = o.operations.map((rawOp, i): Operation => {
    const opWhere = `${where} operations[${i}]`;
    if (typeof rawOp !== "object" || rawOp === null) fail(opWhere, "must be an object");
    const op = rawOp as Record<string, unknown>;
    const grants = asStringArray(opWhere, "grants", op.grants);
    if (grants.length > MAX_GRANTS) fail(opWhere, `"grants" has ${grants.length} entries (max ${MAX_GRANTS})`);
    let match: HttpMatch | GraphqlMatch | undefined;
    if (op.match !== undefined) {
      if (typeof op.match !== "object" || op.match === null) fail(opWhere, '"match" must be an object');
      const m = op.match as Record<string, unknown>;
      // ADR 0059: a match is EITHER an HTTP match (method/path) OR a GraphQL match
      // (operation/field). Discriminate by which keys are present; reject a mix.
      const isGraphql = m.operation !== undefined || m.field !== undefined;
      const isHttp = m.method !== undefined || m.path !== undefined;
      if (isGraphql && isHttp) {
        fail(opWhere, '"match" must be an HTTP match (method/path) OR a GraphQL match (operation/field), not both');
      }
      if (isGraphql) {
        if (m.operation !== "query" && m.operation !== "mutation" && m.operation !== "subscription") {
          fail(opWhere, '"match.operation" must be "query", "mutation", or "subscription"');
        }
        if (typeof m.field !== "string" || !GRAPHQL_FIELD_RE.test(m.field)) {
          fail(opWhere, '"match.field" must be a GraphQL field name ([A-Za-z_][A-Za-z0-9_]*)');
        }
        match = { operation: m.operation, field: m.field };
      } else {
        if (m.method !== undefined && typeof m.method !== "string") fail(opWhere, '"match.method" must be a string');
        if (m.path !== undefined && typeof m.path !== "string") fail(opWhere, '"match.path" must be a string');
        match = { ...(typeof m.method === "string" ? { method: m.method } : {}), ...(typeof m.path === "string" ? { path: m.path } : {}) };
      }
    }
    let asset: AssetSpec | undefined;
    if (op.asset !== undefined) {
      if (typeof op.asset !== "object" || op.asset === null) fail(opWhere, '"asset" must be an object');
      const a = op.asset as Record<string, unknown>;
      if (typeof a.kind !== "string" || !a.kind) fail(opWhere, '"asset.kind" must be a non-empty string');
      if (a.surface !== "action" && a.surface !== "asset") fail(opWhere, '"asset.surface" must be "action" or "asset"');
      let urlFallback: AssetUrlFallback | undefined;
      if (a.urlFallback !== undefined) {
        if (typeof a.urlFallback !== "object" || a.urlFallback === null) {
          fail(opWhere, '"asset.urlFallback" must be an object');
        }
        const u = a.urlFallback as Record<string, unknown>;
        if (typeof u.pattern !== "string" || !u.pattern) {
          fail(opWhere, '"asset.urlFallback.pattern" must be a non-empty string');
        }
        if (typeof u.fields !== "object" || u.fields === null || Array.isArray(u.fields)) {
          fail(opWhere, '"asset.urlFallback.fields" must be an object of field → template');
        }
        // Developer-error guard: a field template may reference only captures the
        // pattern declares (a typo here would silently derive nothing at runtime —
        // the proxy's URL fallback is deliberately fail-soft).
        const captureNames = (t: string) =>
          [...t.matchAll(/\{([A-Za-z0-9_]+)(?::int)?\}/g)].map((m) => m[1] ?? "");
        const declared = new Set(captureNames(u.pattern));
        for (const [field, template] of Object.entries(u.fields as Record<string, unknown>)) {
          if (typeof template !== "string" || !template) {
            fail(opWhere, `"asset.urlFallback.fields.${field}" must be a non-empty string`);
          }
          for (const name of captureNames(template)) {
            if (!declared.has(name)) {
              fail(opWhere, `"asset.urlFallback.fields.${field}" references {${name}}, which the pattern does not capture`);
            }
          }
        }
        urlFallback = { pattern: u.pattern, fields: u.fields as Record<string, string> };
      }
      if (a.data !== undefined) {
        if (typeof a.data !== "object" || a.data === null || Array.isArray(a.data)) {
          fail(opWhere, '"asset.data" must be an object of field → extractor path(s)');
        }
        for (const [field, v] of Object.entries(a.data as Record<string, unknown>)) {
          const chain = Array.isArray(v) ? v : [v];
          if (chain.length === 0 || !chain.every((p) => typeof p === "string" && p)) {
            fail(opWhere, `"asset.data.${field}" must be a non-empty extractor path or a non-empty array of them`);
          }
        }
      }
      asset = {
        kind: a.kind,
        surface: a.surface,
        ...(a.success !== undefined ? { success: a.success as Record<string, unknown> } : {}),
        ...(a.data !== undefined ? { data: a.data as Record<string, string | string[]> } : {}),
        ...(a.fetchable !== undefined ? { fetchable: a.fetchable as Record<string, string> } : {}),
        ...(urlFallback ? { urlFallback } : {}),
      };
    }
    return { grants, ...(match ? { match } : {}), ...(asset ? { asset } : {}) };
  });

  const display = parseDisplay(where, o.display, o.provider);
  const cli = o.cli !== undefined ? parseCli(where, o.cli) : undefined;

  let test: ConnectorTest | undefined;
  if (o.test !== undefined) {
    if (typeof o.test !== "object" || o.test === null) fail(where, '"test" must be an object');
    const t = o.test as Record<string, unknown>;
    if (typeof t.path !== "string" || !t.path.startsWith("/")) {
      fail(where, '"test.path" must be a string starting with "/"');
    }
    if (t.method !== undefined && t.method !== "GET" && t.method !== "POST") {
      fail(where, '"test.method" must be "GET" or "POST"');
    }
    if (t.body !== undefined && (typeof t.body !== "string" || t.body.length > 4096)) {
      fail(where, '"test.body" must be a bounded string');
    }
    test = {
      path: t.path,
      ...(t.method !== undefined ? { method: t.method as "GET" | "POST" } : {}),
      ...(t.body !== undefined ? { body: t.body as string } : {}),
    };
  }

  const oauth = o.oauth !== undefined ? parseOauth(where, o.oauth, hosts) : undefined;
  // ADR 0106 addendum: oauth-facet connectors are OAuth-only. Exactly one
  // inject header, carrying NO secretRef (the value is the store-resolved
  // access token); static connectors require a secretRef on every header.
  if (oauth !== undefined) {
    if (credential.source !== "inject") {
      fail(where, 'an "oauth" connector must use "credential.source": "inject"');
    }
    if (credential.injects.length !== 1) {
      fail(where, 'an "oauth" connector must declare exactly one credential.injects entry');
    }
    if (credential.injects[0]!.secretRef !== undefined) {
      fail(where, 'an "oauth" connector\'s inject must not carry a "secretRef" (the token lives in the credential store)');
    }
  } else if (credential.source === "inject") {
    for (const [i, inj] of credential.injects.entries()) {
      if (inj.secretRef === undefined) {
        fail(where, `credential.injects[${i}] "secretRef" is required for a connector without an "oauth" facet`);
      }
    }
  }

  // ADR 0115: optional user-scoped credential support. Purely additive — the
  // org credential above is untouched. Invariants:
  //  - at least one mode (oauth / token);
  //  - inject connectors need exactly ONE header (the user value renders
  //    through it) and must NOT carry a `userCredential.inject`;
  //  - mint connectors are token-only and MUST carry `userCredential.inject`
  //    (the mint engine renders the org header, so user mode needs its own);
  //  - `oauth: true` requires the top-level oauth facet.
  let userCredential: UserCredentialFacet | undefined;
  if (o.userCredential !== undefined) {
    const uw = `${where} userCredential`;
    if (typeof o.userCredential !== "object" || o.userCredential === null) {
      fail(uw, "must be an object");
    }
    const uc = o.userCredential as Record<string, unknown>;
    let userOauth: true | UserOauthOverrides | undefined;
    if (uc.oauth !== undefined) {
      if (uc.oauth === true) {
        userOauth = true;
      } else if (typeof uc.oauth === "object" && uc.oauth !== null && !Array.isArray(uc.oauth)) {
        const raw = uc.oauth as Record<string, unknown>;
        const overrides: UserOauthOverrides = {};
        if (raw.scopes !== undefined) {
          const scopes = asStringArray(uw, "oauth.scopes", raw.scopes);
          if (scopes.length > 50) fail(uw, '"oauth.scopes" has too many entries (max 50)');
          if (scopes.some((s) => !s || /\s/.test(s))) {
            fail(uw, '"oauth.scopes" entries must be non-empty and whitespace-free');
          }
          overrides.scopes = scopes;
        }
        if (raw.scopesParam !== undefined) {
          if (typeof raw.scopesParam !== "string" || !/^[a-z_]{1,32}$/.test(raw.scopesParam)) {
            fail(uw, '"oauth.scopesParam" must be a short lowercase identifier');
          }
          overrides.scopesParam = raw.scopesParam;
        }
        if (raw.grantPath !== undefined) {
          if (
            typeof raw.grantPath !== "string" ||
            raw.grantPath.length === 0 ||
            raw.grantPath.length > 64 ||
            raw.grantPath
              .split(".")
              .some(
                (seg) =>
                  !OAUTH_METADATA_SEGMENT_RE.test(seg) || UNSAFE_OBJECT_PATH_SEGMENTS.has(seg),
              ) ||
            raw.grantPath.split(".").length > MAX_OAUTH_METADATA_PATH_SEGMENTS
          ) {
            fail(uw, '"oauth.grantPath" must be a short dot-path');
          }
          overrides.grantPath = raw.grantPath;
        }
        if (raw.authorizeParams !== undefined) {
          if (
            typeof raw.authorizeParams !== "object" ||
            raw.authorizeParams === null ||
            Array.isArray(raw.authorizeParams)
          ) {
            fail(uw, '"oauth.authorizeParams" must be an object of param → value');
          }
          const entries = Object.entries(raw.authorizeParams as Record<string, unknown>);
          if (entries.length > MAX_OAUTH_EXTRA_PARAMS) {
            fail(uw, `"oauth.authorizeParams" has ${entries.length} entries (max ${MAX_OAUTH_EXTRA_PARAMS})`);
          }
          const params: Record<string, string> = {};
          for (const [k, v] of entries) {
            if (RESERVED_OAUTH_PARAMS.has(k)) fail(uw, `"oauth.authorizeParams" key "${k}" is reserved`);
            if (
              !k ||
              k.length > MAX_OAUTH_PARAM_LENGTH ||
              typeof v !== "string" ||
              v.length > MAX_OAUTH_PARAM_LENGTH ||
              /[\r\n\0]/.test(k) ||
              /[\r\n\0]/.test(v)
            ) {
              fail(uw, '"oauth.authorizeParams" entries must be short, control-free strings');
            }
            params[k] = v;
          }
          overrides.authorizeParams = params;
        }
        if (raw.metadata !== undefined) {
          if (typeof raw.metadata !== "object" || raw.metadata === null) {
            fail(uw, '"oauth.metadata" must be an object');
          }
          const m = raw.metadata as Record<string, unknown>;
          if (m.probe !== undefined) {
            fail(uw, '"oauth.metadata.probe" is not supported on the user override (facet-only)');
          }
          if (m.fromTokenResponse === undefined) {
            fail(uw, '"oauth.metadata" must carry "fromTokenResponse"');
          }
          if (
            typeof m.fromTokenResponse !== "object" ||
            m.fromTokenResponse === null ||
            Array.isArray(m.fromTokenResponse)
          ) {
            fail(uw, '"oauth.metadata.fromTokenResponse" must be an object of field → dot-path');
          }
          const map: Partial<Record<OauthMetadataField, string>> = {};
          for (const [k, path] of Object.entries(m.fromTokenResponse as Record<string, unknown>)) {
            if (!OAUTH_METADATA_FIELDS.has(k)) {
              fail(uw, `"oauth.metadata.fromTokenResponse" key "${k}" is not a metadata field`);
            }
            if (typeof path !== "string" || !path) {
              fail(uw, `"oauth.metadata.fromTokenResponse.${k}" must be a non-empty dot-path`);
            }
            const segments = path.split(".");
            if (segments.length > MAX_OAUTH_METADATA_PATH_SEGMENTS) {
              fail(uw, `"oauth.metadata.fromTokenResponse.${k}" has too many path segments`);
            }
            for (const seg of segments) {
              if (!OAUTH_METADATA_SEGMENT_RE.test(seg) || UNSAFE_OBJECT_PATH_SEGMENTS.has(seg)) {
                fail(uw, `"oauth.metadata.fromTokenResponse.${k}" segment "${seg}" is not allowed`);
              }
            }
            map[k as OauthMetadataField] = path;
          }
          overrides.metadata = { fromTokenResponse: map };
        }
        userOauth = overrides;
      } else {
        fail(uw, '"oauth" must be true or an overrides object');
      }
    }
    let token: { hint: string } | undefined;
    if (uc.token !== undefined) {
      if (typeof uc.token !== "object" || uc.token === null) fail(uw, '"token" must be an object');
      const t = uc.token as Record<string, unknown>;
      if (typeof t.hint !== "string" || !t.hint.trim() || t.hint.length > MAX_DISPLAY_BLURB) {
        fail(uw, `"token.hint" must be a non-empty string (max ${MAX_DISPLAY_BLURB} chars)`);
      }
      token = { hint: t.hint };
    }
    if (userOauth === undefined && token === undefined) {
      fail(uw, 'must declare at least one mode ("oauth" and/or "token")');
    }
    if (userOauth !== undefined && credential.source === "inject" && oauth === undefined) {
      fail(uw, '"oauth" requires the connector\'s top-level "oauth" facet');
    }
    let userInject: { header: string; template: string } | undefined;
    if (uc.inject !== undefined) {
      if (typeof uc.inject !== "object" || uc.inject === null) fail(uw, '"inject" must be an object');
      const inj = uc.inject as Record<string, unknown>;
      if (typeof inj.header !== "string" || !HEADER_NAME_RE.test(inj.header)) {
        fail(uw, '"inject.header" must be a valid HTTP header name');
      }
      if (typeof inj.template !== "string" || !inj.template.includes("{}") || /[\r\n]/.test(inj.template)) {
        fail(uw, '"inject.template" must be a string containing the "{}" placeholder and no newlines');
      }
      userInject = { header: inj.header, template: inj.template };
    }
    if (credential.source === "inject") {
      if (credential.injects.length !== 1) {
        fail(uw, "requires exactly one credential.injects entry (one personal value renders through one header); multi-header connectors cannot declare user support");
      }
      if (userInject !== undefined) {
        fail(uw, '"inject" is only for mint connectors (inject connectors render the user value through their existing header)');
      }
    } else {
      if (userOauth !== undefined) {
        fail(uw, 'mint connectors support "token" mode only (user-to-server OAuth is not implemented)');
      }
      if (userInject === undefined) {
        fail(uw, '"inject" is required on a mint connector (the mint engine renders the org header, so user mode needs its own header spec)');
      }
    }
    userCredential = {
      ...(userOauth !== undefined ? { oauth: userOauth } : {}),
      ...(token ? { token } : {}),
      ...(userInject ? { inject: userInject } : {}),
    };
  }

  const webhook = o.webhook !== undefined ? parseWebhookFacet(where, o.webhook) : undefined;

  // ADR 0059: the GraphQL endpoint (the single path GraphQL ops POST to). Required
  // when any operation has a GraphQL match; defaults to `/graphql`. Validated like
  // a path (absolute, no `..` / control chars) since it widens what the proxy will
  // body-parse + gate.
  let graphqlEndpoint: string | undefined;
  const hasGraphqlOp = operations.some((op) => op.match !== undefined && isGraphqlMatch(op.match));
  if (o.graphqlEndpoint !== undefined) {
    if (
      typeof o.graphqlEndpoint !== "string" ||
      !o.graphqlEndpoint.startsWith("/") ||
      o.graphqlEndpoint.includes("..") ||
      /[\s\r\n\0]/.test(o.graphqlEndpoint)
    ) {
      fail(where, '"graphqlEndpoint" must be an absolute path ("/…") with no "..", whitespace, or control chars');
    }
    graphqlEndpoint = o.graphqlEndpoint;
  } else if (hasGraphqlOp) {
    graphqlEndpoint = "/graphql";
  }

  return {
    provider: o.provider,
    protocol: "http",
    credential,
    hosts,
    ...(graphqlEndpoint ? { graphqlEndpoint } : {}),
    operations,
    display,
    ...(cli ? { cli } : {}),
    ...(test ? { test } : {}),
    ...(oauth ? { oauth } : {}),
    ...(userCredential ? { userCredential } : {}),
    ...(webhook ? { webhook } : {}),
  };
}

/** Build the provider→connector map; throws on a duplicate provider. */
export function buildRegistry(connectors: Connector[]): Map<string, Connector> {
  const map = new Map<string, Connector>();
  for (const c of connectors) {
    if (map.has(c.provider)) throw new Error(`duplicate connector provider "${c.provider}"`);
    map.set(c.provider, c);
  }
  return map;
}

// ---------------------------------------------------------------------------
// On-disk registry (lazy; the connector JSON files are siblings of this module)
// ---------------------------------------------------------------------------

let cachedRegistry: Map<string, Connector> | null = null;

function loadFromDisk(): Map<string, Connector> {
  const dir = import.meta.dir;
  const files = readdirSync(dir).filter((f) => f.endsWith(".json"));
  const connectors = files.map((f) => {
    let raw: unknown;
    try {
      raw = JSON.parse(readFileSync(join(dir, f), "utf8"));
    } catch (e) {
      throw new Error(`connector ${f}: invalid JSON — ${(e as Error).message}`);
    }
    return parseConnector(raw, f);
  });
  return buildRegistry(connectors);
}

/** The built-in connector seeds (parsed + validated once, then cached). The
 * sync, file-only registry — the default for the pure fns + the test fixtures.
 * Production callers use {@link loadRegistry} so they also see admin-authored
 * connectors. */
export function connectorRegistry(): Map<string, Connector> {
  if (cachedRegistry === null) cachedRegistry = loadFromDisk();
  return cachedRegistry;
}

// ---------------------------------------------------------------------------
// Full registry = built-in seeds ∪ admin-authored (DB) connectors (ADR 0057 C1)
// ---------------------------------------------------------------------------

/** Source of custom (DB-backed) connectors. `ConnectorStore` satisfies this;
 * tests pass a fake. Kept structural so this module stays DB-agnostic. */
export interface CustomConnectorSource {
  list(): Promise<ReadonlyArray<{ provider: string; config: unknown }>>;
}

let mergedRegistry: Map<string, Connector> | null = null;

/**
 * The full connector registry: built-in file seeds ∪ admin-authored DB
 * connectors, validated + cached until {@link invalidateRegistry}.
 *
 * Built-ins take precedence — a custom row whose provider collides with a seed
 * is ignored (the write path rejects collisions up front; this is the defensive
 * belt). A custom row that fails `parseConnector`, or whose `config.provider`
 * mismatches its row key, is skipped + logged — never fatal, so one bad row
 * can't break session-create fleet-wide. A DB-fetch failure degrades to
 * seeds-only rather than failing the request.
 */
export async function loadRegistry(source: CustomConnectorSource): Promise<Map<string, Connector>> {
  if (mergedRegistry !== null) return mergedRegistry;
  // Copy the seed map so we never mutate the cached built-in registry.
  const map = new Map(connectorRegistry());
  let custom: ReadonlyArray<{ provider: string; config: unknown }>;
  try {
    custom = await source.list();
  } catch (e) {
    console.error(`loadRegistry: custom-connector fetch failed, using built-in seeds only — ${(e as Error).message}`);
    mergedRegistry = map;
    return map;
  }
  for (const row of custom) {
    if (map.has(row.provider)) {
      console.error(`loadRegistry: custom connector "${row.provider}" shadows a built-in seed; ignored`);
      continue;
    }
    let parsed: Connector;
    try {
      parsed = parseConnector(row.config, `db:${row.provider}`);
    } catch (e) {
      console.error(`loadRegistry: skipping invalid custom connector "${row.provider}" — ${(e as Error).message}`);
      continue;
    }
    if (parsed.provider !== row.provider) {
      console.error(`loadRegistry: custom connector row "${row.provider}" has config.provider "${parsed.provider}"; ignored`);
      continue;
    }
    map.set(parsed.provider, parsed);
  }
  mergedRegistry = map;
  return mergedRegistry;
}

/** Drop the cached merged registry. Call after any connector write (C3) so the
 * next {@link loadRegistry} re-reads the DB. The seed cache is untouched. */
export function invalidateRegistry(): void {
  mergedRegistry = null;
}

// ---------------------------------------------------------------------------
// The two consumers: validation + compile
// ---------------------------------------------------------------------------

/** Is `provider:action` granted by some operation of the provider's connector? */
export function grantsCapability(
  provider: string,
  action: string,
  registry: Map<string, Connector> = connectorRegistry(),
): boolean {
  const c = registry.get(provider);
  if (!c) return false;
  return c.operations.some((op) => op.grants.includes(action));
}

/**
 * Compile structured connection grants → the per-session IntegrationPolicy.
 *
 * For each capability, activate the operations whose `grants` include its
 * `action`, and for each activated operation emit:
 *   - an **inject** (Plane B) if the connector's credential source is `inject`
 *     (host-gated + method/path-gated from the op's `match`); and
 *   - an **observe** if the operation declares an `asset` spec — *regardless of
 *     credential source*, since observation is orthogonal to auth (a mint
 *     provider like GitHub still surfaces assets from its responses).
 *
 * Unknown providers/actions are skipped (profile-save validation already
 * rejected them; a clamp-drop here is the defensive belt). Exact-duplicate
 * entries are deduped.
 */
export function compileIntegrationPolicy(
  grants: readonly IntegrationGrantSelection[],
  registry: Map<string, Connector> = connectorRegistry(),
  inputs?: SessionPolicyInputs,
): IntegrationPolicyJson {
  const injects: IntegrationInjectJson[] = [];
  const observes: IntegrationObserveJson[] = [];
  const seenInject = new Set<string>();
  const seenObserve = new Set<string>();
  // ADR 0057: hosts opened by a granted power (folded into the egress allow-list
  // below — you must be able to REACH a host you inject a credential onto).
  const grantedHosts = new Set<string>();
  for (const grant of grants) {
    const connector = registry.get(grant.provider);
    if (!connector) continue;
    for (const op of connector.operations) {
      if (!op.grants.includes(grant.operation)) continue;
      for (const h of connector.hosts) grantedHosts.add(h);
      // ADR 0059: a GraphQL op gates `POST <graphqlEndpoint>` and is body-matched
      // by (operation, field); a REST op gates by (method, path glob). The path
      // glob is emitted whole (the proxy globs `*` over the full request path —
      // previously truncated at the first `*`, which over-matched siblings, e.g.
      // `/repos/*/pulls` collapsing to `/repos/` and firing on `/repos/o/r/git/refs`).
      let methods: string[];
      let path_globs: string[];
      let graphql_operation = "";
      let graphql_field = "";
      if (op.match && isGraphqlMatch(op.match)) {
        methods = ["POST"];
        path_globs = [connector.graphqlEndpoint ?? "/graphql"];
        graphql_operation = op.match.operation;
        graphql_field = op.match.field;
      } else {
        methods = op.match?.method ? [op.match.method.toUpperCase()] : [];
        path_globs = op.match?.path ? [op.match.path] : [];
      }

      // ADR 0115: a user-scoped grant on a human compile (userSubjectId set)
      // swaps the VALUE SOURCE to the launching user's sealed credential; the
      // gating and the header shape are unchanged. Programmatic sessions never
      // supply a subject, so they compile the org credential below.
      const userScoped =
        grant.userScoped === true &&
        inputs?.userSubjectId !== undefined &&
        connector.userCredential !== undefined;
      const userMintSource = (): CredentialMintSourceJson => ({
        oauth_user: {
          user_id: inputs?.userSubjectId ?? "",
          connection_id: grant.connectionId,
          provider: connector.provider,
        },
      });

      if (connector.credential.source === "inject") {
        // One egress inject per declared header (most connectors have one; e.g.
        // Datadog `pup` injects DD-API-KEY AND DD-APPLICATION-KEY). An
        // oauth-facet connector's single header has no secretRef: the value is
        // the store-resolved OAuth token, carried as a brokered source so the
        // proxy's refresh rail keeps it fresh (ADR 0106 addendum).
        for (const inj of connector.credential.injects) {
          const entry: IntegrationInjectJson = {
            hosts: connector.hosts,
            header_name: inj.header,
            header_template: inj.template ?? "{}",
            secret_ref: userScoped ? "" : (inj.secretRef ?? ""),
            mint_source: userScoped
              ? userMintSource()
              : connector.oauth
                ? {
                    oauth_connector: {
                      connection_id: grant.connectionId,
                      provider: connector.provider,
                    },
                  }
                : null,
            methods,
            path_globs,
            graphql_operation,
            graphql_field,
          };
          const key = JSON.stringify(entry);
          if (!seenInject.has(key)) {
            seenInject.add(key);
            injects.push(entry);
          }
        }
      }

      // ADR 0056 amendment: a mint connector rides the SAME egress inject plane —
      // the coordinator mints the value (scoped to caps) instead of resolving a
      // static secret, and the *integration* renders the header (scheme is the
      // provider's, not hardcoded here). So we emit only the GATING + the mint
      // marker; `header_name`/`header_template` are filled coordinator-side.
      if (connector.credential.source === "mint") {
        // ADR 0115: user mode on a mint connector renders through the facet's
        // OWN header spec (the mint engine renders the org header, so parse
        // requires `userCredential.inject` on mint connectors).
        const userInject = userScoped ? connector.userCredential?.inject : undefined;
        const entry: IntegrationInjectJson = {
          hosts: connector.hosts,
          header_name: userInject?.header ?? "",
          header_template: userInject?.template ?? "",
          secret_ref: "",
          mint_source: userInject
            ? userMintSource()
            : {
                connection: {
                  connection_id: grant.connectionId,
                  provider: connector.provider,
                },
              },
          methods,
          path_globs,
          graphql_operation,
          graphql_field,
        };
        const key = JSON.stringify(entry);
        if (!seenInject.has(key)) {
          seenInject.add(key);
          injects.push(entry);
        }
      }

      if (op.asset) {
        const a = op.asset;
        const statusClass = a.success?.statusClass;
        const fetchableExternal = a.fetchable?.external;
        const entry: IntegrationObserveJson = {
          hosts: connector.hosts,
          methods,
          path_globs,
          provider: connector.provider,
          asset_kind: a.kind,
          surface: a.surface,
          success_status_class: typeof statusClass === "string" ? statusClass : null,
          // ADR 0059: a GraphQL asset gates success on the absence of top-level
          // `errors` (the connector sets `success: { noGraphqlErrors: true }`).
          success_no_graphql_errors: a.success?.noGraphqlErrors === true,
          graphql_operation,
          graphql_field,
          // A `string[]` data value is a fallback chain — flattened to repeated
          // `[field, path]` pairs; the proxy takes the first path that resolves.
          data: Object.entries(a.data ?? {}).flatMap(([field, v]): [string, string][] =>
            (Array.isArray(v) ? v : [v]).map((path) => [field, path]),
          ),
          fetchable: typeof fetchableExternal === "string" ? fetchableExternal : null,
          url_fallback: a.urlFallback
            ? { pattern: a.urlFallback.pattern, fields: Object.entries(a.urlFallback.fields) }
            : null,
        };
        const key = JSON.stringify(entry);
        if (!seenObserve.has(key)) {
          seenObserve.add(key);
          observes.push(entry);
        }
      }
    }
  }
  // ADR 0057: carry the profile's network + secrets in the same policy. The
  // coordinator sources the egress policy's network + secret injection from
  // here (the secret VALUES are resolved host-side from `secret_ref`).
  const network: IntegrationNetworkJson = {
    default: inputs?.network?.default === "allow" ? "allow" : "deny",
    // ADR 0057: union the profile's hand-typed allow-list with every granted
    // connector's hosts (deduped, admin entries first). Granting a power opens
    // its host's egress — matching what the profile UI already shows as "hosts
    // opened by granted powers" (`derivedHosts`). Without this, a profile that
    // grants a capability but doesn't *also* re-type the host gets a DNS "could
    // not resolve host" at runtime despite the credential injection being wired.
    allow_hosts: [...new Set([...(inputs?.network?.allowHosts ?? []), ...grantedHosts])],
    allow_host_patterns: inputs?.network?.allowHostPatterns ?? [],
  };
  const secrets: IntegrationSecretJson[] = (inputs?.secrets ?? []).map((s) => ({
    secret_ref: s.ref,
    env_var: s.envVar,
    mode: s.mode === "literal" ? "literal" : "broker",
    allow_hosts: s.allowHosts ?? [],
    allow_host_patterns: s.allowHostPatterns ?? [],
  }));
  return { injects, observes, network, secrets, guest_services: [], tunnels: [] };
}

// ---------------------------------------------------------------------------
// CLI integration plan (ADR 0058) — the per-session CLI artifact
// ---------------------------------------------------------------------------

/** The shared integrations CLI bundle (a dynamic-mount skill name) that carries
 * every `binSource: "bundled"` CLI. Enabling any bundled-CLI integration adds this
 * one bundle to the session's selected skills — one `dyn_*` slot for all CLIs. */
export const INTEGRATIONS_CLI_BUNDLE = "integrations-cli";

/** One enabled CLI provider — the input the per-session discovery skill renders. */
export interface EnabledCli {
  provider: string;
  displayName: string;
  bins: string[];
  doc: string;
}

/** The per-session CLI artifact compiled from a profile's capabilities. */
export interface CliIntegrationPlan {
  /** Harmless dummy env agentd sets so each CLI's local auth gate passes — the
   * real credential is supplied host-side by the egress proxy. Merged across
   * providers (last writer wins on a key collision; authors avoid clashing). */
  dummyEnv: Record<string, string>;
  /** Stub config files agentd writes for the same purpose. */
  dummyFiles: CliDummyFile[];
  /** Enabled CLI providers, sorted by provider (the discovery-skill input). */
  enabled: EnabledCli[];
  /** Dynamic-mount bundle names this plan requires (the shared integrations CLI
   * bundle, if any bundled CLI is enabled). The orchestrator unions these into the
   * session's selected skills. */
  bundles: string[];
}

/**
 * Compile a profile's bound capabilities → the per-session {@link CliIntegrationPlan}.
 *
 * A connector's CLI is *enabled* iff the profile holds ≥1 capability the connector
 * actually grants (same gating as {@link compileIntegrationPolicy} — a granted
 * `provider:action`). For each enabled CLI we collect its dummy env/files (so the
 * tool stops gating on local auth), its bins + doc (for discovery), and flag the
 * shared bundle. Auth itself is unchanged: the proxy already injects the real
 * header (static *or* minted) for the connector's hosts.
 */
export function compileCliIntegrations(
  capabilities: string[],
  registry: Map<string, Connector> = connectorRegistry(),
): CliIntegrationPlan {
  const grantedProviders = new Set<string>();
  for (const capStr of capabilities) {
    const cap = parseCapability(capStr);
    if (!cap) continue;
    const connector = registry.get(cap.provider);
    if (!connector) continue;
    if (connector.operations.some((op) => op.grants.includes(cap.action))) {
      grantedProviders.add(cap.provider);
    }
  }

  const dummyEnv: Record<string, string> = {};
  const dummyFiles: CliDummyFile[] = [];
  const enabled: EnabledCli[] = [];
  // Dedup'd mount bundles (a `dyn_*` slot each): the shared integrations bundle
  // once (any CLI enables it — it carries the discovery helper + SKILL.md), plus
  // each uploaded connector's own catalog bundle (the binary itself).
  const bundles = new Set<string>();

  for (const provider of [...grantedProviders].sort()) {
    const cli = registry.get(provider)!.cli;
    if (!cli) continue;
    enabled.push({ provider, displayName: registry.get(provider)!.display.name, bins: cli.bins, doc: cli.doc });
    bundles.add(INTEGRATIONS_CLI_BUNDLE);
    if (cli.binSource === "uploaded" && cli.bundle) bundles.add(cli.bundle);
    for (const [k, v] of Object.entries(cli.dummyEnv ?? {})) dummyEnv[k] = v;
    for (const f of cli.dummyFiles ?? []) dummyFiles.push(f);
  }

  return { dummyEnv, dummyFiles, enabled, bundles: [...bundles] };
}

// ---------------------------------------------------------------------------
// Connected-status derivation + the member-safe provider catalog (redesign #1)
// ---------------------------------------------------------------------------

export type ConnectorStatus = "connected" | "available" | "needs_reconnect";

/** The coordinator's derived credential lifecycle for an OAuth connector
 * (`OAuthCredentialMeta.status`). */
export type OauthCredentialStatus = "connected" | "expired" | "broken" | "revoked";

/**
 * Whether a connector's credential is configured (*connected*), not yet
 * (*available*), or terminally rejected (*needs_reconnect*). Pure — the caller
 * supplies the org-secret name set, mint requirements, and (for oauth-facet
 * connectors) the coordinator's per-provider credential status:
 *   - oauth  → status comes SOLELY from the sealed credential store. No row ⇒
 *              available; broken/revoked/expired ⇒ needs_reconnect; connected ⇒
 *              connected. `expired` is degraded, not transient: the scanner
 *              refreshes max(30 min, 25% TTL) AHEAD of expiry, so a row only
 *              reaches `expired` after hours of failed refreshes — guest
 *              requests are 401ing by then. Self-healing: the row stays in
 *              the due set; a successful refresh restores `connected`.
 *   - inject → connected ⇔ every `secretRef` exists in the org secret store.
 *   - mint   → connected ⇔ every required mint field exists as an org secret
 *              (caller derives the names as `${kind}.${field}` from the
 *              coordinator's mint-kind registry). None given ⇒ available.
 */
export function connectorStatus(
  connector: Connector,
  orgSecretNames: ReadonlySet<string>,
  requiredMintSecretNames: ReadonlyArray<string> = [],
  oauthStatus?: OauthCredentialStatus,
): ConnectorStatus {
  if (connector.oauth) {
    if (oauthStatus === undefined) return "available";
    return oauthStatus === "connected" ? "connected" : "needs_reconnect";
  }
  if (connector.credential.source === "inject") {
    // Connected ⇔ every injected header's secret is present in the org store.
    return connector.credential.injects.every((i) => i.secretRef !== undefined && orgSecretNames.has(i.secretRef))
      ? "connected"
      : "available";
  }
  if (requiredMintSecretNames.length === 0) return "available";
  return requiredMintSecretNames.every((n) => orgSecretNames.has(n)) ? "connected" : "available";
}

export type CatalogAccess = "read" | "write";

/** One grantable power, derived for display (the slug stays canonical). */
export interface CatalogCapability {
  /** The capability `action` (e.g. `issues:write`). */
  action: string;
  /** Derived from the op's HTTP method (GET/HEAD/OPTIONS → read, else write). */
  access: CatalogAccess;
  /** The asset kind this op surfaces, if any (e.g. `pull_request`). */
  asset?: string;
  /** Operator-facing name, for a provider that serves its own operations. */
  label?: string;
  /** The exact host a curated operation calls. */
  host?: string;
  /** For a host-less operation, the endpoint kind that makes it usable. */
  endpointRule?: "google-api" | "non-google-api";
}

/** Member-safe view of one connector — display identity + the powers it grants +
 * the hosts it opens. Carries NO secretRef / header / template / mint kind, so it
 * is safe to expose to non-admins (the Launch receipt + in-session provenance). */
export interface ProviderCatalogEntry {
  provider: string;
  display: ConnectorDisplay;
  credentialSource: "mint" | "inject";
  hosts: string[];
  capabilities: CatalogCapability[];
  /**
   * "singleton" — one org-wide credential slot; "named" — an administrator
   * configures connections, each its own authority (ADR 0109). The web used to
   * decide this by hard-coding the one provider it knew was named.
   */
  connectionModel: "singleton" | "named";
  /** ADR 0115: user-scoped credential support, when declared. `tokenHint` is
   * the member-facing setup line for PAT mode. Drives the profile editor's
   * per-integration toggle and the Settings → Credentials cards. */
  userCredential?: { oauth: boolean; token: boolean; tokenHint?: string };
}

/** GET/HEAD/OPTIONS → read; otherwise write (a method-less op is conservatively write). */
function accessOf(method: string | undefined): CatalogAccess {
  const m = (method ?? "").trim().toUpperCase();
  return m === "GET" || m === "HEAD" || m === "OPTIONS" ? "read" : "write";
}

/**
 * Project the registry into the member-safe provider catalog. Powers are the
 * union of every operation's `grants`, deduped by action (an asset spec on any
 * op for that action is kept), each tagged with its derived read/write access.
 * Sorted by provider.
 */
export function buildProviderCatalog(
  registry: Map<string, Connector>,
  providers: ReadonlyMap<string, ConnectionProvider>,
): ProviderCatalogEntry[] {
  const entries: ProviderCatalogEntry[] = [];
  for (const connector of registry.values()) {
    const byAction = new Map<string, CatalogCapability>();
    for (const op of connector.operations) {
      // ADR 0059: a GraphQL op's access derives from its operation type
      // (query → read, mutation/subscription → write); a REST op from its method.
      const access: CatalogAccess =
        op.match && isGraphqlMatch(op.match)
          ? op.match.operation === "query"
            ? "read"
            : "write"
          : accessOf(op.match?.method);
      for (const action of op.grants) {
        const existing = byAction.get(action);
        if (existing) {
          if (existing.asset === undefined && op.asset) existing.asset = op.asset.kind;
          continue;
        }
        byAction.set(action, { action, access, ...(op.asset ? { asset: op.asset.kind } : {}) });
      }
    }
    entries.push({
      provider: connector.provider,
      display: connector.display,
      credentialSource: connector.credential.source,
      hosts: connector.hosts,
      capabilities: [...byAction.values()],
      connectionModel: "singleton",
      ...(connector.userCredential
        ? {
            userCredential: {
              oauth: connector.userCredential.oauth !== undefined,
              token: connector.userCredential.token !== undefined,
              ...(connector.userCredential.token
                ? { tokenHint: connector.userCredential.token.hint }
                : {}),
            },
          }
        : {}),
    });
  }
  // A named-connection provider is not a connector: it has no org-wide
  // credential and its operations come from its own catalog. Serving it here
  // means the web renders every integration from ONE response, instead of
  // pushing in a hand-written entry for the provider it happens to know.
  for (const provider of providers.values()) {
    entries.push({
      provider: provider.key,
      display: {
        name: provider.displayName,
        blurb: provider.blurb,
        category: provider.category,
        icon: { mono: defaultIconMono(provider.key), color: defaultIconColor(provider.key) },
      },
      credentialSource: "mint",
      hosts: [
        ...new Set(
          provider.operations.describe
            .map((operation) => operation.host)
            .filter((host): host is string => host !== null),
        ),
      ],
      capabilities: provider.operations.describe.map((operation) => ({
        action: operation.action,
        access: operation.access,
        label: operation.label,
        ...(operation.host ? { host: operation.host } : {}),
        ...(operation.endpointRule ? { endpointRule: operation.endpointRule } : {}),
      })),
      connectionModel: "named",
    });
  }
  entries.sort((a, b) => a.provider.localeCompare(b.provider));
  return entries;
}
