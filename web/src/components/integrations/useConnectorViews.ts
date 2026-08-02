/**
 * Joins the three reads the integrations surfaces need into one `ConnectorView`
 * per provider: the member catalog (display identity + powers + hosts + logo),
 * the admin connector list (connected/available status + builtin), and the
 * profiles list (how many grant this provider's powers). Built-ins ∪ custom.
 */

import {
  useConnectors,
  useIntegrationCatalog,
  useIntegrationConnections,
} from "@/hooks/useIntegrations";
import { useProfiles } from "@/hooks/useProfiles";
import { fallbackIdentity, type Access } from "@/lib/connectorModel";
import { builtinLogo } from "@/lib/connectorLogos";
import type { ProviderCatalogEntry } from "@/gen/engram/app/v1/integration_pb";
import { GOOGLE_CLOUD_OPERATIONS, GOOGLE_CLOUD_PROVIDER } from "./googleCloud";

export interface ConnectorCapabilityView {
  action: string;
  access: Access;
  asset?: string;
}

export interface ConnectorView {
  provider: string;
  defaultConnectionId: string;
  name: string;
  category: string;
  blurb: string;
  icon: { mono: string; color: string; logo?: string };
  credentialSource: "mint" | "inject";
  hosts: string[];
  capabilities: ConnectorCapabilityView[];
  status: "connected" | "available";
  builtin: boolean;
  /** How many profiles grant ≥1 of this provider's powers. */
  usedBy: number;
  /** Profile {id,name,icon} that grant this provider's powers. */
  usedByProfiles: { id: string; name: string; icon: string }[];
  /** Named connections exist for providers such as Google Cloud. */
  connectionModel?: "named";
  connectionCount?: number;
}

export interface ConnectorViewsResult {
  views: ConnectorView[];
  isLoading: boolean;
  error: unknown;
}

export function useConnectorViews(): ConnectorViewsResult {
  const cat = useIntegrationCatalog();
  const conns = useConnectors();
  const namedConnections = useIntegrationConnections();
  const profiles = useProfiles();

  const rowByProvider = new Map((conns.data?.connectors ?? []).map((c) => [c.provider, c]));
  const allProfiles = profiles.data?.profiles ?? [];

  const views: ConnectorView[] = (cat.data?.providers ?? []).map((e) => {
    const fb = fallbackIdentity(e.provider);
    const row = rowByProvider.get(e.provider);
    const used = allProfiles.filter((p) =>
      (p.integrationGrants ?? []).some((grant) => grant.connectionId === e.defaultConnectionId),
    );
    // Logo precedence (matches useProviderIdentity): bundled built-in →
    // uploaded overlay → monogram.
    const logo = builtinLogo(e.provider) ?? (e.display?.icon?.logo || undefined);
    return {
      provider: e.provider,
      defaultConnectionId: e.defaultConnectionId,
      name: e.display?.name || fb.name,
      category: e.display?.category || fb.category,
      blurb: e.display?.blurb ?? "",
      icon: {
        mono: e.display?.icon?.mono || fb.icon.mono,
        color: e.display?.icon?.color || fb.icon.color,
        ...(logo ? { logo } : {}),
      },
      credentialSource: e.credentialSource === "mint" ? "mint" : "inject",
      hosts: e.hosts,
      capabilities: e.capabilities.map((c) => ({
        action: c.action,
        access: c.access === "write" ? "write" : "read",
        ...(c.asset ? { asset: c.asset } : {}),
      })),
      status: row?.status === "connected" ? "connected" : "available",
      builtin: row?.builtin ?? false,
      usedBy: used.length,
      usedByProfiles: used.map((p) => ({ id: p.id, name: p.name, icon: p.icon })),
    };
  });

  const googleConnections = (namedConnections.data?.connections ?? []).filter(
    (connection) => connection.provider === GOOGLE_CLOUD_PROVIDER,
  );
  views.push(googleCloudView(googleConnections, allProfiles));

  return {
    views,
    isLoading: cat.isLoading || conns.isLoading || namedConnections.isLoading,
    error: cat.error ?? conns.error ?? namedConnections.error,
  };
}

interface GrantingProfileLike {
  id: string;
  name: string;
  icon: string;
  integrationGrants?: { connectionId: string }[];
}

interface GoogleConnectionLike {
  id: string;
  googleCloud?: { endpoints: string[] };
}

/**
 * The synthetic Google Cloud view (ADR 0109 named connections). Google has no
 * row in the connector catalog yet, so both view joins build its entry here —
 * with per-connection data when the caller can list connections (admin), and
 * a placeholder host when it cannot (member surfaces).
 */
function googleCloudView(
  connections: readonly GoogleConnectionLike[],
  profiles: readonly GrantingProfileLike[],
): ConnectorView {
  const connectionIds = new Set(connections.map((connection) => connection.id));
  const granting = profiles.filter((profile) =>
    (profile.integrationGrants ?? []).some((grant) => connectionIds.has(grant.connectionId)),
  );
  const hosts = [
    ...new Set(connections.flatMap((connection) => connection.googleCloud?.endpoints ?? [])),
  ];
  return {
    provider: GOOGLE_CLOUD_PROVIDER,
    defaultConnectionId: "",
    name: "Google Cloud",
    category: "Infrastructure",
    blurb:
      "Run gcloud against approved Google Cloud APIs through keyless Workload Identity Federation. Credentials remain outside the session.",
    icon: { mono: "GC", color: "#4285f4" },
    credentialSource: "mint",
    hosts: hosts.length > 0 ? hosts : ["googleapis.com"],
    capabilities: GOOGLE_CLOUD_OPERATIONS.map(({ action, access }) => ({ action, access })),
    status: connections.length > 0 ? "connected" : "available",
    builtin: true,
    usedBy: granting.length,
    usedByProfiles: granting.map((profile) => ({
      id: profile.id,
      name: profile.name,
      icon: profile.icon,
    })),
    connectionModel: "named",
    connectionCount: connections.length,
  };
}

/** Write-count helper shared by the cards + detail. */
export function writeCount(caps: ConnectorCapabilityView[]): number {
  return caps.filter((c) => c.access === "write").length;
}

/**
 * Map the member catalog (GetIntegrationCatalog) into `ConnectorView`s without
 * the admin-only connector list — for member-facing surfaces (the Launch receipt,
 * session-event icons) where `status`/`builtin`/`usedBy` don't apply. Status is
 * reported "available" (unused by these surfaces).
 */
export function catalogToViews(providers: ProviderCatalogEntry[]): ConnectorView[] {
  const views = providers.map((e): ConnectorView => {
    const fb = fallbackIdentity(e.provider);
    const logo = builtinLogo(e.provider) ?? (e.display?.icon?.logo || undefined);
    return {
      provider: e.provider,
      defaultConnectionId: e.defaultConnectionId,
      name: e.display?.name || fb.name,
      category: e.display?.category || fb.category,
      blurb: e.display?.blurb ?? "",
      icon: {
        mono: e.display?.icon?.mono || fb.icon.mono,
        color: e.display?.icon?.color || fb.icon.color,
        ...(logo ? { logo } : {}),
      },
      credentialSource: e.credentialSource === "mint" ? "mint" : "inject",
      hosts: e.hosts,
      capabilities: e.capabilities.map((c) => ({
        action: c.action,
        access: c.access === "write" ? "write" : "read",
        ...(c.asset ? { asset: c.asset } : {}),
      })),
      status: "available" as const,
      builtin: false,
      usedBy: 0,
      usedByProfiles: [],
    };
  });
  // Members cannot list named connections, so the launch receipt gets the
  // placeholder-host Google view; it still names the powers a Google grant
  // opens instead of dropping them (web-M1).
  views.push(googleCloudView([], []));
  return views;
}
