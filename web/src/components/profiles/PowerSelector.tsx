/**
 * PowerSelector — the capability-first power picker for one connected connector.
 * Collapses a provider's capabilities into one row per resource (`pulls:read` +
 * `pulls:write` → a "Pull requests" row with Read/Write pills), with a filter, an
 * "N on" count, All-reads / Clear bulk actions, and a scroll cap so it scales to
 * dozens. Each pill toggles a single `provider:action` capability.
 */

import { useState } from "react";
import { CheckIcon, EyeIcon, PencilIcon, SearchIcon } from "lucide-react";

import { Input } from "@/components/ui/input";
import { Text } from "@/components/ui/text";
import { cn } from "@/lib/utils";
import { resourceLabel, resourceOf } from "@/lib/connectorModel";
import type {
  ConnectorCapabilityView,
  ConnectorView,
} from "@/components/integrations/useConnectorViews";

interface ResourceGroup {
  resource: string;
  label: string;
  read?: ConnectorCapabilityView;
  write?: ConnectorCapabilityView;
  asset?: string;
}

export function PowerSelector({
  view,
  isOn,
  onToggle,
}: {
  view: ConnectorView;
  /** Whether `provider:action` is currently granted. */
  isOn: (action: string) => boolean;
  /** Grant/revoke a single `provider:action`. */
  onToggle: (action: string, on: boolean) => void;
}) {
  const [q, setQ] = useState("");

  const groups: ResourceGroup[] = [];
  const byResource = new Map<string, ResourceGroup>();
  for (const cap of view.capabilities) {
    const resource = resourceOf(cap.action);
    let g = byResource.get(resource);
    if (!g) {
      g = { resource, label: resourceLabel(resource) };
      byResource.set(resource, g);
      groups.push(g);
    }
    if (cap.access === "write") g.write = cap;
    else g.read = cap;
    if (cap.asset) g.asset = cap.asset;
  }

  const ql = q.trim().toLowerCase();
  const shown = ql
    ? groups.filter((g) => g.label.toLowerCase().includes(ql) || g.resource.includes(ql))
    : groups;
  const granted = view.capabilities.filter((c) => isOn(c.action)).length;
  const reads = view.capabilities.filter((c) => c.access === "read");

  return (
    <div>
      <div className="flex items-center gap-2.5 px-3.5 py-2">
        <div className="relative flex-1">
          <SearchIcon className="absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            className="h-8 pl-8 text-sm"
            placeholder={`Filter ${groups.length} powers`}
            value={q}
            onChange={(e) => setQ(e.target.value)}
          />
        </div>
        <Text variant="label" tone="muted" className="text-[0.56rem] whitespace-nowrap">
          {granted} on
        </Text>
        <button
          type="button"
          className="font-display text-[0.64rem] font-semibold tracking-[0.06em] text-primary uppercase"
          onClick={() => reads.forEach((c) => onToggle(c.action, true))}
        >
          All reads
        </button>
        {granted > 0 && (
          <button
            type="button"
            className="font-display text-[0.64rem] font-semibold tracking-[0.06em] text-primary uppercase"
            onClick={() => view.capabilities.forEach((c) => onToggle(c.action, false))}
          >
            Clear
          </button>
        )}
      </div>
      <div className="max-h-72 overflow-y-auto border-t border-border/60">
        {shown.map((g) => {
          const readOn = g.read ? isOn(g.read.action) : false;
          const writeOn = g.write ? isOn(g.write.action) : false;
          return (
            <div
              key={g.resource}
              className="flex items-center gap-2.5 border-t border-border/40 px-3.5 py-1.5 first:border-t-0"
            >
              <span
                className={cn(
                  "flex-1 text-sm",
                  readOn || writeOn ? "text-foreground" : "text-muted-foreground",
                )}
              >
                {g.label}
              </span>
              {g.asset && (
                <span className="text-[0.66rem] text-muted-foreground">
                  {g.asset.replace(/_/g, " ")}
                </span>
              )}
              <div className="flex gap-1.5">
                {g.read && (
                  <AccessPill
                    label="Read"
                    icon="read"
                    on={readOn}
                    onClick={() => onToggle(g.read!.action, !readOn)}
                  />
                )}
                {g.write && (
                  <AccessPill
                    label="Write"
                    icon="write"
                    on={writeOn}
                    onClick={() => onToggle(g.write!.action, !writeOn)}
                  />
                )}
              </div>
            </div>
          );
        })}
        {shown.length === 0 && (
          <div className="px-3.5 py-4 text-center text-sm text-muted-foreground">
            No powers match "{q}".
          </div>
        )}
      </div>
    </div>
  );
}

function AccessPill({
  label,
  icon,
  on,
  onClick,
}: {
  label: string;
  icon: "read" | "write";
  on: boolean;
  onClick: () => void;
}) {
  const write = icon === "write";
  const Icon = on ? CheckIcon : write ? PencilIcon : EyeIcon;
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={on}
      aria-label={`${on ? "Granted" : "Grant"} ${label.toLowerCase()}`}
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 font-display text-[0.6rem] font-semibold tracking-[0.07em] uppercase transition-colors",
        on
          ? write
            ? "border-instrument-caution/55 bg-instrument-caution/15 text-foreground"
            : "border-primary/55 bg-primary/15 text-foreground"
          : "border-border text-muted-foreground hover:text-foreground",
      )}
    >
      <Icon
        className={cn(
          "size-3",
          on && write && "text-instrument-caution",
          on && !write && "text-primary",
        )}
      />
      {label}
    </button>
  );
}
