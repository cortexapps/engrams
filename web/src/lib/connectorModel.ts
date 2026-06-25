/**
 * Client-side connector model (Integrations & Profiles redesign).
 *
 * Two jobs:
 *  1. Deterministic identity defaults — a mirror of the orchestrator's
 *     `connectors/registry.ts` (defaultIcon{Mono,Color} / defaultDisplayName) so a
 *     provider with no catalog entry (e.g. a session event naming a since-deleted
 *     connector) still renders the SAME monogram + tint the server would have.
 *  2. Display derivations — `accessOf` (read/write from HTTP method),
 *     `humanizeAction`, `resourceOf`/`resourceLabel`, and `parseConnectorConfig`
 *     (the admin path: a connector's raw `config_json` → display + powers +
 *     credential posture). The slug stays canonical; labels are derived here.
 */

export type Access = "read" | "write";

export interface IconIdentity {
  mono: string;
  color: string;
  /** Serve URL of an uploaded logo (renderer falls back to the monogram). */
  logo?: string;
}

export interface ProviderIdentity {
  provider: string;
  name: string;
  category: string;
  blurb: string;
  icon: IconIdentity;
}

// --- deterministic defaults (kept in sync with the orchestrator) ------------

/** Default tint palette — must match `DEFAULT_ICON_PALETTE` in registry.ts. */
const DEFAULT_ICON_PALETTE = [
  "#1f2328",
  "#632ca6",
  "#362d59",
  "#06ac38",
  "#4a154b",
  "#0052cc",
  "#5e6ad2",
  "#4c4a73",
  "#b8324f",
  "#c4622d",
  "#2a6f6f",
  "#3a5a40",
];

/** FNV-1a 32-bit — must match the orchestrator's `hashString`. */
function hashString(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function defaultIconColor(provider: string): string {
  return DEFAULT_ICON_PALETTE[hashString(provider) % DEFAULT_ICON_PALETTE.length]!;
}

export function defaultIconMono(provider: string): string {
  const alnum = provider.replace(/[^a-z0-9]/gi, "");
  return (alnum.slice(0, 2) || "?").toUpperCase();
}

export function defaultDisplayName(provider: string): string {
  const words = provider.split(/[-_]+/).filter(Boolean);
  return words.length === 0
    ? provider
    : words.map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
}

/** A complete fallback identity for a provider with no catalog entry. */
export function fallbackIdentity(provider: string): ProviderIdentity {
  return {
    provider,
    name: defaultDisplayName(provider),
    category: "Other",
    blurb: "",
    icon: { mono: defaultIconMono(provider), color: defaultIconColor(provider) },
  };
}

// --- display derivations ----------------------------------------------------

/** read for GET/HEAD/OPTIONS, write otherwise (matches the orchestrator). */
export function accessOf(method: string | undefined): Access {
  const m = (method ?? "").trim().toUpperCase();
  return m === "GET" || m === "HEAD" || m === "OPTIONS" ? "read" : "write";
}

/** The resource namespace of an action (`pulls:write` → `pulls`). */
export function resourceOf(action: string): string {
  return action.replace(/:(read|write)$/, "");
}

/** Curated human labels for the common resource namespaces. */
const RESOURCE_LABELS: Record<string, string> = {
  metadata: "Repository metadata",
  contents: "Contents",
  commits: "Commits",
  branches: "Branches",
  pulls: "Pull requests",
  issues: "Issues",
  checks: "Checks",
  actions: "Actions & workflows",
  deployments: "Deployments",
  statuses: "Commit statuses",
  releases: "Releases",
  packages: "Packages",
  projects: "Projects",
  webhooks: "Webhooks",
  members: "Members",
  logs: "Logs",
  metrics: "Metrics",
  monitors: "Monitors",
  events: "Error events",
  incidents: "Incidents",
  oncall: "On-call",
  channels: "Channels",
  chat: "Messages",
  vulnerabilities: "Vulnerabilities",
};

/** A human label for a resource namespace (`pulls` → "Pull requests"). */
export function resourceLabel(resource: string): string {
  const known = RESOURCE_LABELS[resource];
  if (known) return known;
  return resource
    .split(/[_:]/)
    .map((w, i) => (i === 0 ? w.charAt(0).toUpperCase() + w.slice(1) : w))
    .join(" ");
}

/** A human label for a full action slug (`issues:write` → "Write issues"). */
export function humanizeAction(action: string): string {
  const parts = action.split(":").map((s) => s.replace(/_/g, " "));
  if (parts.length === 2) {
    const [res, verb] = parts;
    return `${verb!.charAt(0).toUpperCase()}${verb!.slice(1)} ${res}`;
  }
  return action.replace(/[:_]/g, " ");
}

// --- admin path: parse a connector's raw config_json ------------------------

export interface ParsedCapability {
  action: string;
  access: Access;
  asset?: string;
}

/** One injected header backed by its own org secret (ADR 0058: a connector may
 * inject several, e.g. Datadog's DD-API-KEY + DD-APPLICATION-KEY). */
export interface ParsedInject {
  header: string;
  secretRef: string;
  template: string;
}

/** ADR 0058: the optional CLI facet a connector drives (display posture). */
export interface ParsedCli {
  /** PATH command names this connector contributes. */
  bins: string[];
  /** `bundled` (shared bundle), `uploaded` (admin-uploaded catalog bundle), `npx`. */
  binSource: "bundled" | "uploaded" | "npx";
  /** uploaded only: the mount_catalog bundle carrying the binary. */
  bundle?: string;
}

/** OAuth acquisition facet — the app-credential org secrets the "Add to X" flow
 * seeds before redirecting (the access token itself is obtained server-side). */
export interface ParsedOauth {
  clientIdRef: string;
  clientSecretRef: string;
  /** Bot scopes the token exchange requests — drives the Slack app manifest (ADR 0059). */
  scopes: string[];
  /** Org secret for the app's webhook-signing secret, when the provider has an inbound
   * webhook surface (ADR 0059 Slack triggers). Present → the connect flow seals it. */
  signingSecretRef?: string;
}

export interface ParsedConnectorConfig {
  provider: string;
  credentialSource: "mint" | "inject";
  hosts: string[];
  display: { name: string; category: string; blurb: string; icon: { mono: string; color: string } };
  /** inject only — one or more headers, each backed by an org secret. */
  injects?: ParsedInject[];
  /** mint only */
  mintKind?: string;
  /** present when the connector is connected via an OAuth flow (e.g. Slack). */
  oauth?: ParsedOauth;
  capabilities: ParsedCapability[];
  /** ADR 0058: the CLI this connector drives, if any. */
  cli?: ParsedCli;
}

interface RawOp {
  grants?: string[];
  match?: { method?: string; path?: string };
  asset?: { kind?: string };
}

/**
 * Parse a connector's raw `config_json` (as returned by ListConnectors) into the
 * display + powers + credential posture the admin surfaces render. Display fields
 * default off the provider id exactly as the orchestrator's `parseConnector` does.
 */
export function parseConnectorConfig(configJson: string, provider: string): ParsedConnectorConfig {
  let raw: Record<string, unknown> = {};
  try {
    raw = JSON.parse(configJson) as Record<string, unknown>;
  } catch {
    raw = {};
  }
  const cred = (raw.credential ?? {}) as {
    source?: string;
    injects?: Array<Record<string, string>>;
    mint?: { kind?: string };
  };
  const credentialSource: "mint" | "inject" = cred.source === "mint" ? "mint" : "inject";
  const oauthRaw = raw.oauth as
    | {
        clientIdRef?: string;
        clientSecretRef?: string;
        signingSecretRef?: unknown;
        scopes?: unknown;
      }
    | undefined;
  const oauth =
    oauthRaw?.clientIdRef && oauthRaw?.clientSecretRef
      ? {
          clientIdRef: oauthRaw.clientIdRef,
          clientSecretRef: oauthRaw.clientSecretRef,
          scopes: Array.isArray(oauthRaw.scopes)
            ? oauthRaw.scopes.filter((s): s is string => typeof s === "string")
            : [],
          ...(typeof oauthRaw.signingSecretRef === "string"
            ? { signingSecretRef: oauthRaw.signingSecretRef }
            : {}),
        }
      : undefined;
  const d = (raw.display ?? {}) as {
    name?: string;
    category?: string;
    blurb?: string;
    icon?: { mono?: string; color?: string };
  };

  const capabilities: ParsedCapability[] = [];
  const seen = new Set<string>();
  for (const op of (raw.operations as RawOp[] | undefined) ?? []) {
    const access = accessOf(op.match?.method);
    for (const action of op.grants ?? []) {
      if (seen.has(action)) {
        if (op.asset?.kind) {
          const existing = capabilities.find((c) => c.action === action);
          if (existing && existing.asset === undefined) existing.asset = op.asset.kind;
        }
        continue;
      }
      seen.add(action);
      capabilities.push({ action, access, ...(op.asset?.kind ? { asset: op.asset.kind } : {}) });
    }
  }

  const rawCli = raw.cli as Record<string, unknown> | undefined;
  const cli: ParsedCli | undefined =
    rawCli && Array.isArray(rawCli.bins)
      ? {
          bins: (rawCli.bins as unknown[]).filter((b): b is string => typeof b === "string"),
          binSource:
            rawCli.binSource === "uploaded" || rawCli.binSource === "npx"
              ? rawCli.binSource
              : "bundled",
          ...(typeof rawCli.bundle === "string" ? { bundle: rawCli.bundle } : {}),
        }
      : undefined;

  return {
    provider,
    credentialSource,
    hosts: Array.isArray(raw.hosts) ? (raw.hosts as string[]) : [],
    display: {
      name: d.name ?? defaultDisplayName(provider),
      category: d.category ?? "Other",
      blurb: d.blurb ?? "",
      icon: {
        mono: d.icon?.mono ?? defaultIconMono(provider),
        color: d.icon?.color ?? defaultIconColor(provider),
      },
    },
    ...(credentialSource === "inject"
      ? {
          injects: (cred.injects ?? []).map((i) => ({
            header: i.header ?? "",
            secretRef: i.secretRef ?? "",
            template: i.template ?? "{}",
          })),
        }
      : { mintKind: cred.mint?.kind }),
    ...(oauth ? { oauth } : {}),
    capabilities,
    ...(cli ? { cli } : {}),
  };
}
