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
 * boundary. Only `http` is implemented; `grpc`/`graphql` are designed-in shapes
 * (ADR §3) a later phase adds as new host-proxy parsers.
 *
 * This phase compiles only Plane-B *injects* (credential source = inject). Mint
 * connectors (`source: "mint"`, e.g. GitHub) validate + gate capabilities now;
 * their mint scopes are compiled in Phase 5.
 */

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// ---------------------------------------------------------------------------
// Connector types (the static JSON shape)
// ---------------------------------------------------------------------------

/** `{}` is replaced by the resolved secret value host-side (e.g. `"Bearer {}"`). */
export interface InjectCredential {
  source: "inject";
  inject: { header: string; secretRef: string; template?: string };
}
export interface MintCredential {
  source: "mint";
  mint: { kind: string };
}
export type Credential = InjectCredential | MintCredential;

/** HTTP request match — the one protocol-shaped field (ADR §3). */
export interface HttpMatch {
  method?: string;
  path?: string;
}

/** Response→asset map (consumed in Phase 4; validated + carried now). */
export interface AssetSpec {
  kind: string;
  surface: "action" | "asset";
  success?: Record<string, unknown>;
  data?: Record<string, string>;
  fetchable?: Record<string, string>;
}

export interface Operation {
  /** Capability `action`s that activate this operation. */
  grants: string[];
  match?: HttpMatch;
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

export interface Connector {
  provider: string;
  /** Only `"http"` is implemented; other values are rejected at load. */
  protocol: "http";
  credential: Credential;
  hosts: string[];
  operations: Operation[];
  /** Always defaulted from `provider` when absent (see {@link parseConnector}). */
  display: ConnectorDisplay;
}

// ---------------------------------------------------------------------------
// Compiled policy (the wire artifact — mirrors engram_core::types::IntegrationPolicy)
// ---------------------------------------------------------------------------

/** One Plane-B injection, snake_case to match the Rust serde shape. */
export interface IntegrationInjectJson {
  hosts: string[];
  header_name: string;
  header_template: string;
  secret_ref: string;
  methods: string[];
  path_prefixes: string[];
}
/** One response-observation spec, snake_case to match the Rust serde shape. */
export interface IntegrationObserveJson {
  hosts: string[];
  methods: string[];
  path_prefixes: string[];
  provider: string;
  asset_kind: string;
  surface: string;
  success_status_class: string | null;
  /** `[field, extractorPath]` pairs (serde `Vec<(String, String)>`). */
  data: [string, string][];
  fetchable: string | null;
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
}

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
}

/** Whether a compiled policy carries anything worth shipping on CreateSession. */
export function policyHasContent(p: IntegrationPolicyJson): boolean {
  return (
    p.injects.length > 0 ||
    p.observes.length > 0 ||
    p.secrets.length > 0 ||
    p.network.allow_hosts.length > 0 ||
    p.network.allow_host_patterns.length > 0 ||
    p.network.default === "allow"
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
/** RFC 7230 header field-name token (no spaces, colons, or CR/LF). */
const HEADER_NAME_RE = /^[A-Za-z0-9!#$%&'*+.^_`|~-]+$/;
/** Provider id: a lowercase identifier (matches the built-ins). */
const PROVIDER_RE = /^[a-z0-9][a-z0-9_-]*$/;

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

/** Validate + narrow one raw connector object. Throws Error on any malformation. */
export function parseConnector(raw: unknown, where: string): Connector {
  if (typeof raw !== "object" || raw === null) fail(where, "must be a JSON object");
  const o = raw as Record<string, unknown>;

  if (typeof o.provider !== "string" || !o.provider) fail(where, '"provider" must be a non-empty string');
  if (!PROVIDER_RE.test(o.provider)) fail(where, `"provider" "${o.provider}" must be a lowercase identifier ([a-z0-9][a-z0-9_-]*)`);
  if (o.protocol !== "http") fail(where, `"protocol" must be "http" (got ${JSON.stringify(o.protocol)}); grpc/graphql are not yet implemented`);

  const cred = o.credential as Record<string, unknown> | undefined;
  if (typeof cred !== "object" || cred === null) fail(where, '"credential" must be an object');
  let credential: Credential;
  if (cred.source === "inject") {
    const inj = cred.inject as Record<string, unknown> | undefined;
    if (typeof inj !== "object" || inj === null) fail(where, '"credential.inject" must be an object');
    if (typeof inj.header !== "string" || !inj.header) fail(where, '"credential.inject.header" must be a non-empty string');
    if (!HEADER_NAME_RE.test(inj.header)) fail(where, `"credential.inject.header" "${inj.header}" is not a valid HTTP header name`);
    if (typeof inj.secretRef !== "string" || !inj.secretRef) fail(where, '"credential.inject.secretRef" must be a non-empty string');
    if (/\s/.test(inj.secretRef)) fail(where, '"credential.inject.secretRef" must not contain whitespace');
    if (inj.template !== undefined) {
      if (typeof inj.template !== "string") fail(where, '"credential.inject.template" must be a string');
      if (/[\r\n]/.test(inj.template)) fail(where, '"credential.inject.template" must not contain newlines');
      if (!inj.template.includes("{}")) fail(where, '"credential.inject.template" must contain the "{}" value placeholder');
    }
    credential = { source: "inject", inject: { header: inj.header, secretRef: inj.secretRef, ...(typeof inj.template === "string" ? { template: inj.template } : {}) } };
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
    let match: HttpMatch | undefined;
    if (op.match !== undefined) {
      if (typeof op.match !== "object" || op.match === null) fail(opWhere, '"match" must be an object');
      const m = op.match as Record<string, unknown>;
      if (m.method !== undefined && typeof m.method !== "string") fail(opWhere, '"match.method" must be a string');
      if (m.path !== undefined && typeof m.path !== "string") fail(opWhere, '"match.path" must be a string');
      match = { ...(typeof m.method === "string" ? { method: m.method } : {}), ...(typeof m.path === "string" ? { path: m.path } : {}) };
    }
    let asset: AssetSpec | undefined;
    if (op.asset !== undefined) {
      if (typeof op.asset !== "object" || op.asset === null) fail(opWhere, '"asset" must be an object');
      const a = op.asset as Record<string, unknown>;
      if (typeof a.kind !== "string" || !a.kind) fail(opWhere, '"asset.kind" must be a non-empty string');
      if (a.surface !== "action" && a.surface !== "asset") fail(opWhere, '"asset.surface" must be "action" or "asset"');
      asset = {
        kind: a.kind,
        surface: a.surface,
        ...(a.success !== undefined ? { success: a.success as Record<string, unknown> } : {}),
        ...(a.data !== undefined ? { data: a.data as Record<string, string> } : {}),
        ...(a.fetchable !== undefined ? { fetchable: a.fetchable as Record<string, string> } : {}),
      };
    }
    return { grants, ...(match ? { match } : {}), ...(asset ? { asset } : {}) };
  });

  const display = parseDisplay(where, o.display, o.provider);

  return { provider: o.provider, protocol: "http", credential, hosts, operations, display };
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

/** Glob `path` → prefix the proxy can match (everything before the first `*`). */
function pathPrefix(p: string): string {
  const star = p.indexOf("*");
  return star === -1 ? p : p.slice(0, star);
}

/**
 * Compile a profile's bound capabilities → the per-session IntegrationPolicy.
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
  capabilities: string[],
  registry: Map<string, Connector> = connectorRegistry(),
  inputs?: SessionPolicyInputs,
): IntegrationPolicyJson {
  const injects: IntegrationInjectJson[] = [];
  const observes: IntegrationObserveJson[] = [];
  const seenInject = new Set<string>();
  const seenObserve = new Set<string>();
  for (const capStr of capabilities) {
    const cap = parseCapability(capStr);
    if (!cap) continue;
    const connector = registry.get(cap.provider);
    if (!connector) continue;
    for (const op of connector.operations) {
      if (!op.grants.includes(cap.action)) continue;
      const methods = op.match?.method ? [op.match.method.toUpperCase()] : [];
      const path_prefixes = op.match?.path ? [pathPrefix(op.match.path)] : [];

      if (connector.credential.source === "inject") {
        const inj = connector.credential.inject;
        const entry: IntegrationInjectJson = {
          hosts: connector.hosts,
          header_name: inj.header,
          header_template: inj.template ?? "{}",
          secret_ref: inj.secretRef,
          methods,
          path_prefixes,
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
          path_prefixes,
          provider: connector.provider,
          asset_kind: a.kind,
          surface: a.surface,
          success_status_class: typeof statusClass === "string" ? statusClass : null,
          data: Object.entries(a.data ?? {}),
          fetchable: typeof fetchableExternal === "string" ? fetchableExternal : null,
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
    allow_hosts: inputs?.network?.allowHosts ?? [],
    allow_host_patterns: inputs?.network?.allowHostPatterns ?? [],
  };
  const secrets: IntegrationSecretJson[] = (inputs?.secrets ?? []).map((s) => ({
    secret_ref: s.ref,
    env_var: s.envVar,
    mode: s.mode === "literal" ? "literal" : "broker",
    allow_hosts: s.allowHosts ?? [],
    allow_host_patterns: s.allowHostPatterns ?? [],
  }));
  return { injects, observes, network, secrets };
}

// ---------------------------------------------------------------------------
// Connected-status derivation + the member-safe provider catalog (redesign #1)
// ---------------------------------------------------------------------------

export type ConnectorStatus = "connected" | "available";

/**
 * Whether a connector's credential is configured (*connected*) or not yet
 * (*available*). Pure — the caller supplies the org-secret name set and, for a
 * mint connector, the org-secret names its mint kind requires:
 *   - inject → connected ⇔ the `secretRef` exists in the org secret store.
 *   - mint   → connected ⇔ every required mint field exists as an org secret
 *              (caller derives the names as `${kind}.${field}` from the
 *              coordinator's mint-kind registry). None given ⇒ available.
 */
export function connectorStatus(
  connector: Connector,
  orgSecretNames: ReadonlySet<string>,
  requiredMintSecretNames: ReadonlyArray<string> = [],
): ConnectorStatus {
  if (connector.credential.source === "inject") {
    return orgSecretNames.has(connector.credential.inject.secretRef) ? "connected" : "available";
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
export function buildProviderCatalog(registry: Map<string, Connector>): ProviderCatalogEntry[] {
  const entries: ProviderCatalogEntry[] = [];
  for (const connector of registry.values()) {
    const byAction = new Map<string, CatalogCapability>();
    for (const op of connector.operations) {
      const access = accessOf(op.match?.method);
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
    });
  }
  entries.sort((a, b) => a.provider.localeCompare(b.provider));
  return entries;
}
