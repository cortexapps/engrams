/**
 * Catalog-driven capability picker (ADR 0057 D1).
 *
 * Replaces the profile editor's free-text capabilities field. The grantable
 * `provider:action` set is derived from the connector catalog (`useConnectors`)
 * — an admin toggles the ones a profile grants instead of typing them, so a
 * capability no connector backs can't be entered by accident (the orchestrator
 * still re-validates server-side). A selected capability may carry an optional
 * `@resource` scope. Capabilities already on the profile that no current
 * connector grants ("orphans" — e.g. a removed connector) stay visible + removable
 * rather than silently dropped.
 */

import { useConnectors } from "../../hooks/useIntegrations";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

/** `provider:action[@resource]` → its base (`provider:action`) + resource. */
function parseCap(cap: string): { base: string; resource: string | null } {
  const at = cap.indexOf("@");
  return at === -1
    ? { base: cap, resource: null }
    : { base: cap.slice(0, at), resource: cap.slice(at + 1) };
}

/** The `provider:action` capabilities a connector's operations grant. */
function grantsOf(configJson: string, provider: string): string[] {
  try {
    const c = JSON.parse(configJson) as { operations?: { grants?: string[] }[] };
    const out = new Set<string>();
    for (const op of c.operations ?? []) {
      for (const g of op.grants ?? []) {
        if (g) out.add(`${provider}:${g}`);
      }
    }
    return [...out];
  } catch {
    return [];
  }
}

export function CapabilityPicker({
  value,
  onChange,
}: {
  value: string[];
  onChange: (caps: string[]) => void;
}) {
  const { data, isLoading, error } = useConnectors();
  const connectors = data?.connectors ?? [];

  // provider → sorted base caps it grants (from the catalog).
  const byProvider = new Map<string, Set<string>>();
  for (const c of connectors) {
    const bases = grantsOf(c.configJson, c.provider);
    if (bases.length === 0) continue;
    const set = byProvider.get(c.provider) ?? new Set<string>();
    bases.forEach((b) => set.add(b));
    byProvider.set(c.provider, set);
  }
  const providers = [...byProvider.keys()].sort();
  const grantable = new Set([...byProvider.values()].flatMap((s) => [...s]));

  // base → resource for the profile's current caps.
  const selected = new Map(value.map((v) => [parseCap(v).base, parseCap(v).resource] as const));
  const orphans = value.filter((v) => !grantable.has(parseCap(v).base));

  const setCap = (base: string, on: boolean, resource: string | null) => {
    const rest = value.filter((v) => parseCap(v).base !== base);
    if (!on) {
      onChange(rest);
      return;
    }
    const cap = resource && resource.trim() ? `${base}@${resource.trim()}` : base;
    onChange([...rest, cap]);
  };

  if (isLoading) return <p className="text-sm text-muted-foreground">Loading catalog…</p>;
  if (error)
    return <p className="text-sm text-destructive">could not load connectors — {String(error)}</p>;

  return (
    <div className="space-y-4">
      {providers.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No connectors grant capabilities yet. Add one under{" "}
          <span className="font-medium">Settings → Integrations</span>.
        </p>
      ) : (
        providers.map((provider) => (
          <div key={provider} className="space-y-2">
            <div className="font-mono text-xs font-medium text-muted-foreground">{provider}</div>
            {[...byProvider.get(provider)!].sort().map((base) => {
              const action = base.slice(provider.length + 1);
              const on = selected.has(base);
              const resource = selected.get(base) ?? null;
              return (
                <div key={base} className="flex items-center gap-3">
                  <Switch
                    id={`cap-${base}`}
                    checked={on}
                    onCheckedChange={(c) => setCap(base, c, resource)}
                    aria-label={base}
                  />
                  <label htmlFor={`cap-${base}`} className="font-mono text-sm">
                    {action}
                  </label>
                  {on && (
                    <Input
                      className="ml-auto h-7 w-56 font-mono text-xs"
                      placeholder="resource (optional, e.g. owner/repo)"
                      aria-label={`${base} resource`}
                      value={resource ?? ""}
                      onChange={(e) => setCap(base, true, e.target.value)}
                    />
                  )}
                </div>
              );
            })}
          </div>
        ))
      )}

      {orphans.length > 0 && (
        <div className="space-y-1.5">
          <div className="text-xs font-medium text-muted-foreground">
            Not granted by any current connector
          </div>
          <div className="flex flex-wrap gap-2">
            {orphans.map((cap) => (
              <Badge key={cap} variant="outline" className="gap-1 font-mono">
                {cap}
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-4 w-4 p-0"
                  aria-label={`remove ${cap}`}
                  onClick={() => onChange(value.filter((v) => v !== cap))}
                >
                  ✕
                </Button>
              </Badge>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
