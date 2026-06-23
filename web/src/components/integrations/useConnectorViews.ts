/**
 * Joins the three reads the integrations surfaces need into one `ConnectorView`
 * per provider: the member catalog (display identity + powers + hosts + logo),
 * the admin connector list (connected/available status + builtin), and the
 * profiles list (how many grant this provider's powers). Built-ins ∪ custom.
 */

import { useConnectors, useIntegrationCatalog } from "@/hooks/useIntegrations";
import { useProfiles } from "@/hooks/useProfiles";
import { fallbackIdentity, type Access } from "@/lib/connectorModel";

export interface ConnectorCapabilityView {
  action: string;
  access: Access;
  asset?: string;
}

export interface ConnectorView {
  provider: string;
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
}

export interface ConnectorViewsResult {
  views: ConnectorView[];
  isLoading: boolean;
  error: unknown;
}

export function useConnectorViews(): ConnectorViewsResult {
  const cat = useIntegrationCatalog();
  const conns = useConnectors();
  const profiles = useProfiles();

  const rowByProvider = new Map((conns.data?.connectors ?? []).map((c) => [c.provider, c]));
  const allProfiles = profiles.data?.profiles ?? [];

  const views: ConnectorView[] = (cat.data?.providers ?? []).map((e) => {
    const fb = fallbackIdentity(e.provider);
    const row = rowByProvider.get(e.provider);
    const grantsProvider = (caps: string[]) => caps.some((c) => c.startsWith(`${e.provider}:`));
    const used = allProfiles.filter((p) => grantsProvider(p.capabilities ?? []));
    return {
      provider: e.provider,
      name: e.display?.name || fb.name,
      category: e.display?.category || fb.category,
      blurb: e.display?.blurb ?? "",
      icon: {
        mono: e.display?.icon?.mono || fb.icon.mono,
        color: e.display?.icon?.color || fb.icon.color,
        ...(e.display?.icon?.logo ? { logo: e.display.icon.logo } : {}),
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

  return { views, isLoading: cat.isLoading || conns.isLoading, error: cat.error ?? conns.error };
}

/** Write-count helper shared by the cards + detail. */
export function writeCount(caps: ConnectorCapabilityView[]): number {
  return caps.filter((c) => c.access === "write").length;
}
