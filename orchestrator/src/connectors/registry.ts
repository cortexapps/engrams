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

export interface Connector {
  provider: string;
  /** Only `"http"` is implemented; other values are rejected at load. */
  protocol: "http";
  credential: Credential;
  hosts: string[];
  operations: Operation[];
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
export interface IntegrationPolicyJson {
  injects: IntegrationInjectJson[];
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

/** Validate + narrow one raw connector object. Throws Error on any malformation. */
export function parseConnector(raw: unknown, where: string): Connector {
  if (typeof raw !== "object" || raw === null) fail(where, "must be a JSON object");
  const o = raw as Record<string, unknown>;

  if (typeof o.provider !== "string" || !o.provider) fail(where, '"provider" must be a non-empty string');
  if (o.protocol !== "http") fail(where, `"protocol" must be "http" (got ${JSON.stringify(o.protocol)}); grpc/graphql are not yet implemented`);

  const cred = o.credential as Record<string, unknown> | undefined;
  if (typeof cred !== "object" || cred === null) fail(where, '"credential" must be an object');
  let credential: Credential;
  if (cred.source === "inject") {
    const inj = cred.inject as Record<string, unknown> | undefined;
    if (typeof inj !== "object" || inj === null) fail(where, '"credential.inject" must be an object');
    if (typeof inj.header !== "string" || !inj.header) fail(where, '"credential.inject.header" must be a non-empty string');
    if (typeof inj.secretRef !== "string" || !inj.secretRef) fail(where, '"credential.inject.secretRef" must be a non-empty string');
    if (inj.template !== undefined && typeof inj.template !== "string") fail(where, '"credential.inject.template" must be a string');
    credential = { source: "inject", inject: { header: inj.header, secretRef: inj.secretRef, ...(typeof inj.template === "string" ? { template: inj.template } : {}) } };
  } else if (cred.source === "mint") {
    const mint = cred.mint as Record<string, unknown> | undefined;
    if (typeof mint !== "object" || mint === null) fail(where, '"credential.mint" must be an object');
    if (typeof mint.kind !== "string" || !mint.kind) fail(where, '"credential.mint.kind" must be a non-empty string');
    credential = { source: "mint", mint: { kind: mint.kind } };
  } else {
    fail(where, `"credential.source" must be "inject" or "mint" (got ${JSON.stringify(cred.source)})`);
  }

  const hosts = asStringArray(where, "hosts", o.hosts);

  if (!Array.isArray(o.operations)) fail(where, '"operations" must be an array');
  const operations: Operation[] = o.operations.map((rawOp, i): Operation => {
    const opWhere = `${where} operations[${i}]`;
    if (typeof rawOp !== "object" || rawOp === null) fail(opWhere, "must be an object");
    const op = rawOp as Record<string, unknown>;
    const grants = asStringArray(opWhere, "grants", op.grants);
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

  return { provider: o.provider, protocol: "http", credential, hosts, operations };
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

/** The loaded connector registry (parsed + validated once, then cached). */
export function connectorRegistry(): Map<string, Connector> {
  if (cachedRegistry === null) cachedRegistry = loadFromDisk();
  return cachedRegistry;
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
 * For each capability whose provider's connector is credential-source `inject`,
 * activate the operations whose `grants` include the capability's `action`, and
 * emit one inject entry per activated operation (host-gated + method/path-gated
 * from the op's `match`). Mint connectors are skipped (Phase 5). Unknown
 * providers/actions are skipped (profile-save validation already rejected them;
 * a clamp-drop here is the defensive belt). Exact-duplicate injects are deduped.
 */
export function compileIntegrationPolicy(
  capabilities: string[],
  registry: Map<string, Connector> = connectorRegistry(),
): IntegrationPolicyJson {
  const injects: IntegrationInjectJson[] = [];
  const seen = new Set<string>();
  for (const capStr of capabilities) {
    const cap = parseCapability(capStr);
    if (!cap) continue;
    const connector = registry.get(cap.provider);
    if (!connector || connector.credential.source !== "inject") continue;
    const inj = connector.credential.inject;
    for (const op of connector.operations) {
      if (!op.grants.includes(cap.action)) continue;
      const entry: IntegrationInjectJson = {
        hosts: connector.hosts,
        header_name: inj.header,
        header_template: inj.template ?? "{}",
        secret_ref: inj.secretRef,
        methods: op.match?.method ? [op.match.method.toUpperCase()] : [],
        path_prefixes: op.match?.path ? [pathPrefix(op.match.path)] : [],
      };
      const key = JSON.stringify(entry);
      if (!seen.has(key)) {
        seen.add(key);
        injects.push(entry);
      }
    }
  }
  return { injects };
}
