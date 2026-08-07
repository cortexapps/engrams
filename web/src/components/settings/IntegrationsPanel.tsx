/**
 * Integrations marketplace (redesign) — the admin catalog of providers whose
 * powers a profile can grant. A searchable, category-tabbed grid of provider
 * cards split into Connected / Available, a guided Connect sheet, and the
 * power-user custom-connector modal. Per-provider management lives at
 * /settings/integrations/$provider.
 */

import { useMemo, useState } from "react";
import { PlusIcon, SearchIcon, SearchXIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Text } from "@/components/ui/text";
import { ConnectSheet } from "@/components/integrations/ConnectSheet";
import { CustomConnectorModal } from "@/components/integrations/CustomConnectorModal";
import { ProviderCard } from "@/components/integrations/ProviderCard";
import { CategoryFilter } from "@/components/integrations/CategoryFilter";
import { GoogleCloudConnectDialog } from "@/components/integrations/GoogleCloudConnections";
import { useConnectorViews, type ConnectorView } from "@/components/integrations/useConnectorViews";
import { PageHeading } from "../page-heading";

export function IntegrationsPanel() {
  const { views, isLoading, error } = useConnectorViews();
  const [q, setQ] = useState("");
  const [cat, setCat] = useState("all");
  const [connect, setConnect] = useState<string | null>(null);
  const [custom, setCustom] = useState(false);

  const categories = useMemo(() => {
    const seen: string[] = [];
    for (const v of views) if (!seen.includes(v.category)) seen.push(v.category);
    return ["all", ...seen];
  }, [views]);

  // Provider count per category id (+ `all` = total) — the dropdown's live counts.
  const catCounts = useMemo(() => {
    const m: Record<string, number> = { all: views.length };
    for (const v of views) m[v.category] = (m[v.category] ?? 0) + 1;
    return m;
  }, [views]);

  const ql = q.trim().toLowerCase();
  const match = (v: ConnectorView) => {
    if (cat !== "all" && v.category !== cat) return false;
    if (!ql) return true;
    const hay = `${v.name} ${v.category} ${v.blurb} ${v.capabilities.map((c) => c.action).join(" ")}`;
    return hay.toLowerCase().includes(ql);
  };

  const shown = views.filter(match);
  // needs_reconnect is a CONFIGURED connector whose refresh was terminally
  // rejected — it belongs with the connected group, flagged for action.
  const connected = shown.filter((v) => v.status !== "available");
  const available = shown.filter((v) => v.status === "available");
  const connectView = connect ? views.find((v) => v.provider === connect) : undefined;

  return (
    <div className="space-y-6">
      <PageHeading
        title="Integrations"
        actions={
          <Button variant="outline" size="sm" onClick={() => setCustom(true)}>
            <PlusIcon className="size-3.5" />
            Custom connector
          </Button>
        }
      />

      <div className="flex flex-wrap items-center gap-2.5 border-b pb-4">
        <div className="relative min-w-[200px] flex-1">
          <SearchIcon className="absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            className="pl-9"
            placeholder="Search providers and powers"
            value={q}
            onChange={(e) => setQ(e.target.value)}
          />
        </div>
        <CategoryFilter cats={categories} counts={catCounts} active={cat} onChange={setCat} />
      </div>

      {error != null && (
        <p className="text-sm text-destructive">Could not load integrations — {String(error)}</p>
      )}

      {isLoading ? (
        <p className="py-6 text-sm text-muted-foreground">Loading…</p>
      ) : (
        <>
          {connected.length > 0 && (
            <Section label="Connected" count={connected.length}>
              {connected.map((v) => (
                <ProviderCard key={v.provider} view={v} onConnect={setConnect} />
              ))}
            </Section>
          )}
          {available.length > 0 && (
            <Section label="Available" count={available.length}>
              {available.map((v) => (
                <ProviderCard key={v.provider} view={v} onConnect={setConnect} />
              ))}
            </Section>
          )}
          {shown.length === 0 && (
            <div className="py-12 text-center text-muted-foreground">
              <SearchXIcon className="mx-auto size-6 opacity-60" />
              <p className="mt-2 text-sm">No providers match "{q}".</p>
            </div>
          )}
        </>
      )}

      {connectView?.connectionModel === "named" && (
        <GoogleCloudConnectDialog
          open
          onOpenChange={(open) => {
            if (!open) setConnect(null);
          }}
        />
      )}
      {connectView && connectView.connectionModel !== "named" && (
        <ConnectSheet
          view={connectView}
          onClose={() => setConnect(null)}
          onConnected={() => setConnect(null)}
        />
      )}
      {custom && <CustomConnectorModal onClose={() => setCustom(false)} />}
    </div>
  );
}

function Section({
  label,
  count,
  children,
}: {
  label: string;
  count: number;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-3">
      <div className="flex items-baseline gap-2">
        <Text variant="label">{label}</Text>
        <span className="font-mono text-xs text-muted-foreground">{count}</span>
      </div>
      <div
        className="grid gap-3.5"
        style={{ gridTemplateColumns: "repeat(auto-fill, minmax(290px, 1fr))" }}
      >
        {children}
      </div>
    </div>
  );
}
