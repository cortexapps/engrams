/**
 * ProviderCard — one connector in the marketplace grid. Connected cards link to
 * the detail/manage page; available cards open the Connect sheet.
 */

import { Link } from "@tanstack/react-router";
import { GlobeIcon, PencilIcon, PlusIcon, ZapIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { ProviderTile } from "./ProviderTile";
import { StatusDot } from "./chips";
import { writeCount, type ConnectorView } from "./useConnectorViews";

export function ProviderCard({
  view,
  onConnect,
}: {
  view: ConnectorView;
  onConnect: (provider: string) => void;
}) {
  const connected = view.status === "connected";
  const writes = writeCount(view.capabilities);

  return (
    <div className="flex min-h-[168px] flex-col gap-3 rounded-lg border bg-card p-4 shadow-xs">
      <div className="flex items-start gap-3">
        <ProviderTile {...view.icon} name={view.name} size={42} />
        <div className="min-w-0 flex-1">
          <div className="font-display text-[0.98rem] font-semibold [font-stretch:108%]">
            {view.name}
          </div>
          <Text variant="label" tone="muted" className="text-[0.56rem]">
            {view.category}
          </Text>
        </div>
        {connected ? (
          <StatusDot tone="nominal" label="connected" />
        ) : (
          <span className="font-display text-[0.56rem] font-semibold tracking-[0.08em] text-muted-foreground/80 uppercase">
            {view.builtin ? "built-in" : view.credentialSource}
          </span>
        )}
      </div>

      <p className="flex-1 text-[0.82rem] leading-relaxed text-muted-foreground">{view.blurb}</p>

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <ZapIcon className="size-3 opacity-70" />
          {view.capabilities.length} powers
        </span>
        {writes > 0 && (
          <span className="inline-flex items-center gap-1.5">
            <PencilIcon className="size-3 opacity-70" />
            {writes} write
          </span>
        )}
        {view.hosts[0] && (
          <span className="inline-flex items-center gap-1.5">
            <GlobeIcon className="size-3 opacity-70" />
            {view.hosts[0]}
          </span>
        )}
      </div>

      <div className="flex items-center gap-2">
        {connected ? (
          <>
            <Button asChild variant="outline" size="sm" className="flex-1">
              <Link to="/settings/integrations/$provider" params={{ provider: view.provider }}>
                Manage
              </Link>
            </Button>
            {view.usedBy > 0 && (
              <span className="text-xs whitespace-nowrap text-muted-foreground">
                {view.usedBy} profile{view.usedBy === 1 ? "" : "s"}
              </span>
            )}
          </>
        ) : (
          <Button size="sm" className="flex-1" onClick={() => onConnect(view.provider)}>
            <PlusIcon className="size-3.5" />
            Connect
          </Button>
        )}
      </div>
    </div>
  );
}
