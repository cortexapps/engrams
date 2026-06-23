/**
 * Client-side mirror of the orchestrator's `compileIntegrationPolicy`: given a
 * profile draft's capabilities + network + secrets and the connector catalog,
 * derive what a session actually gets — providers used, reachable hosts,
 * credentials, and read/write counts. Powers the Session-policy rail (profile
 * editor) and the Launch "this session will be able to" receipt.
 */

import type {
  ConnectorCapabilityView,
  ConnectorView,
} from "@/components/integrations/useConnectorViews";

export interface DerivedProvider {
  view: ConnectorView;
  caps: ConnectorCapabilityView[];
}

export interface DerivedCredential {
  provider: string | null;
  kind: "mint" | "inject" | "broker" | "literal";
  label: string;
  detail: string;
}

export interface DerivedPolicy {
  providers: DerivedProvider[];
  /** Hosts opened by the granted powers (their connectors' hosts). */
  derivedHosts: string[];
  extraHosts: string[];
  extraPatterns: string[];
  /** Everything a session can reach: derived ∪ extra hosts ∪ patterns. */
  reachable: string[];
  credentials: DerivedCredential[];
  capCount: number;
  writeCount: number;
}

export interface ProfileDraftPolicy {
  capabilities: string[];
  network: { default: "deny" | "allow"; allowHosts: string[]; allowHostPatterns: string[] };
  secrets: { ref: string; envVar: string; mode: "broker" | "literal" }[];
}

/** `provider:action[@resource]` → `{ provider, action }` (resource ignored). */
function splitCap(cap: string): { provider: string; action: string } | null {
  const head = cap.includes("@") ? cap.slice(0, cap.indexOf("@")) : cap;
  const colon = head.indexOf(":");
  if (colon === -1) return null;
  return { provider: head.slice(0, colon), action: head.slice(colon + 1) };
}

export function derivePolicy(draft: ProfileDraftPolicy, views: ConnectorView[]): DerivedPolicy {
  const byProvider = new Map(views.map((v) => [v.provider, v]));
  const used = new Map<string, DerivedProvider>();

  for (const capStr of draft.capabilities) {
    const parsed = splitCap(capStr);
    if (!parsed) continue;
    const view = byProvider.get(parsed.provider);
    if (!view) continue;
    const capDef = view.capabilities.find((c) => c.action === parsed.action);
    if (!capDef) continue;
    const entry = used.get(parsed.provider) ?? { view, caps: [] };
    entry.caps.push(capDef);
    used.set(parsed.provider, entry);
  }

  const providers = [...used.values()];
  const derivedHosts = [...new Set(providers.flatMap((p) => p.view.hosts))];
  const extraHosts = draft.network.allowHosts;
  const extraPatterns = draft.network.allowHostPatterns;

  const credentials: DerivedCredential[] = providers.map((p) =>
    p.view.credentialSource === "mint"
      ? {
          provider: p.view.provider,
          kind: "mint",
          label: `${p.view.name} token`,
          detail: "minted per session",
        }
      : {
          provider: p.view.provider,
          kind: "inject",
          label: `${p.view.name} credential`,
          detail: "brokered · never in sandbox",
        },
  );
  for (const s of draft.secrets) {
    if (!s.envVar) continue;
    credentials.push({
      provider: null,
      kind: s.mode,
      label: s.envVar,
      detail: s.mode === "broker" ? "brokered · never in sandbox" : "literal env value",
    });
  }

  return {
    providers,
    derivedHosts,
    extraHosts,
    extraPatterns,
    reachable: [...derivedHosts, ...extraHosts, ...extraPatterns],
    credentials,
    capCount: draft.capabilities.length,
    writeCount: providers.reduce(
      (n, p) => n + p.caps.filter((c) => c.access === "write").length,
      0,
    ),
  };
}
