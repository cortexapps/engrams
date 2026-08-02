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

/** Narrow the wire's free-form endpoint rule to the two shapes the UI knows. */
function asEndpointRule(value: string): "google-api" | "non-google-api" | undefined {
  return value === "google-api" || value === "non-google-api" ? value : undefined;
}

export interface ConnectorCapabilityView {
  action: string;
  access: Access;
  asset?: string;
  /** Operator-facing name, served by a named-connection provider. */
  label?: string;
  /** The exact host a curated operation calls. */
  host?: string;
  /** For a host-less operation, the endpoint kind that makes it usable. */
  endpointRule?: "google-api" | "non-google-api";
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
  /**
   * "named" — an administrator configures connections, each its own authority
   * (ADR 0109). Served by the catalog; the web used to hard-code the one
   * provider it knew was named.
   */
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

  const connectionsByProvider = new Map<string, { id: string; hosts: string[] }[]>();
  for (const connection of namedConnections.data?.connections ?? []) {
    const hosts = connection.googleCloud?.endpoints ?? [];
    const bucket = connectionsByProvider.get(connection.provider);
    if (bucket) bucket.push({ id: connection.id, hosts });
    else connectionsByProvider.set(connection.provider, [{ id: connection.id, hosts }]);
  }

  const views: ConnectorView[] = (cat.data?.providers ?? []).map((e) => {
    const fb = fallbackIdentity(e.provider);
    const row = rowByProvider.get(e.provider);
    // A named provider has no singleton slot: a profile uses it when it grants
    // ANY of its connections. A singleton provider is used when it grants the
    // one default connection.
    const named = e.connectionModel === "named";
    const connections = connectionsByProvider.get(e.provider) ?? [];
    const connectionIds = new Set(connections.map((connection) => connection.id));
    const used = allProfiles.filter((p) =>
      (p.integrationGrants ?? []).some((grant) =>
        named
          ? connectionIds.has(grant.connectionId)
          : grant.connectionId === e.defaultConnectionId,
      ),
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
      // A named provider's reachable hosts are whatever its connections allow;
      // the catalog's list is the curated superset it could offer.
      hosts:
        named && connections.length > 0
          ? [...new Set(connections.flatMap((connection) => connection.hosts))]
          : e.hosts,
      capabilities: e.capabilities.map((c) => ({
        action: c.action,
        access: c.access === "write" ? "write" : "read",
        ...(c.asset ? { asset: c.asset } : {}),
        ...(c.label ? { label: c.label } : {}),
        ...(c.host ? { host: c.host } : {}),
        ...(c.endpointRule ? { endpointRule: asEndpointRule(c.endpointRule) } : {}),
      })),
      status: named
        ? connections.length > 0
          ? "connected"
          : "available"
        : row?.status === "connected"
          ? "connected"
          : "available",
      builtin: named ? true : (row?.builtin ?? false),
      usedBy: used.length,
      usedByProfiles: used.map((p) => ({ id: p.id, name: p.name, icon: p.icon })),
      ...(named ? { connectionModel: "named" as const, connectionCount: connections.length } : {}),
    };
  });

  return {
    views,
    isLoading: cat.isLoading || conns.isLoading || namedConnections.isLoading,
    error: cat.error ?? conns.error ?? namedConnections.error,
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
        ...(c.label ? { label: c.label } : {}),
        ...(c.host ? { host: c.host } : {}),
        ...(c.endpointRule ? { endpointRule: asEndpointRule(c.endpointRule) } : {}),
      })),
      status: "available" as const,
      builtin: false,
      usedBy: 0,
      usedByProfiles: [],
      ...(e.connectionModel === "named" ? { connectionModel: "named" as const } : {}),
    };
  });
  // A member cannot list named connections, but the catalog still names the
  // powers a grant on one opens — so the launch receipt shows them rather than
  // dropping them (web-M1). No hand-written entry is pushed in any more.
  return views;
}

const isGoogleApiHost = (host: string) => host.endsWith(".googleapis.com");

/**
 * The operations a named connection can actually exercise, given its allowed
 * endpoints.
 *
 * Mirrors the orchestrator's compile-time gate: a curated operation needs its
 * exact `host` in the endpoint list; a host-less one needs an endpoint of the
 * kind its `endpointRule` names. The rules travel WITH the capability now, so
 * this no longer restates a table the orchestrator also keeps.
 */
export function operationsForEndpoints(
  capabilities: readonly ConnectorCapabilityView[],
  endpoints: readonly string[],
): ConnectorCapabilityView[] {
  return capabilities.filter((capability) => {
    if (capability.endpointRule === "google-api") return endpoints.some(isGoogleApiHost);
    if (capability.endpointRule === "non-google-api") {
      return endpoints.some((host) => !isGoogleApiHost(host));
    }
    return capability.host !== undefined && endpoints.includes(capability.host);
  });
}

/** The operator-facing name for an action, or the action itself. */
export function capabilityLabel(
  capabilities: readonly ConnectorCapabilityView[],
  action: string,
): string {
  return capabilities.find((capability) => capability.action === action)?.label ?? action;
}
