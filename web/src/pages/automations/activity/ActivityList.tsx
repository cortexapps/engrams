import { useState, type ReactNode } from "react";

import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

import { relativeAge } from "@/lib/relative-time";
import { StatusDot } from "@/components/status-dot";

import { ActivityEntry } from "./ActivityEntry";
import {
  dropReasonText,
  filterCounts,
  interleaveRuns,
  matchesFilter,
  windowSummary,
  type ActivityDrop,
  type ActivityFilter,
} from "./activity-format";

// The activity ledger shared by the per-automation tab, the cross-automation
// page, and an agent's timeline: pill filters, the "show filtered" switch,
// and one entry per run with the filtered deliveries folded into a quiet row.

export interface ActivityListProps {
  runs: readonly AutomationRunBrief[];
  windows: readonly (FilteredWindow & { automationId?: string })[];
  /** Deliveries that could not fire (no run row): shown in their place in time. */
  drops?: readonly ActivityDrop[];
  isPending: boolean;
  error?: unknown;
  now: number;
  includeFiltered: boolean;
  onIncludeFilteredChange: (next: boolean) => void;
  /** Cross-automation: name each entry's automation. */
  nameOf?: (automationId: string) => string | undefined;
  /** Agent names by instance id, for the chip on a bound run. */
  agentLabelOf?: (instanceId: string) => string | undefined;
  /** A block's kind in an automation, so a step row can say what it did. */
  blockTypeOf?: (automationId: string, blockId: string) => string | undefined;
  /** Show the Superseded filter (per-automation surfaces). */
  superseded?: boolean;
  /** Extra controls on the filter row (an automation picker). */
  controls?: ReactNode;
  /** Entry to open on first render (a deep link). */
  openRunId?: string;
  emptyText?: string;
}

const LABEL: Record<ActivityFilter, string> = {
  all: "All",
  failed: "Failed",
  running: "Running",
  superseded: "Superseded",
};

export function ActivityList({
  runs,
  windows,
  drops = [],
  isPending,
  error,
  now,
  includeFiltered,
  onIncludeFilteredChange,
  nameOf,
  agentLabelOf,
  blockTypeOf,
  superseded = false,
  controls,
  openRunId,
  emptyText = "No activity yet. The next trigger occurrence lands here.",
}: ActivityListProps) {
  const [filter, setFilter] = useState<ActivityFilter>("all");
  const counts = filterCounts(runs);
  const shown = runs.filter((r) => matchesFilter(r, filter));
  const rows = interleaveRuns(
    shown,
    filter === "all" ? windows : [],
    filter === "all" ? drops : [],
  );
  const filters: ActivityFilter[] = superseded
    ? ["all", "failed", "running", "superseded"]
    : ["all", "failed", "running"];

  return (
    <section className="flex flex-col gap-3" aria-label="Activity">
      <div className="flex flex-wrap items-center gap-3">
        <Tabs value={filter} onValueChange={(v) => setFilter(v as ActivityFilter)}>
          <TabsList aria-label="Filter activity">
            {filters.map((f) => (
              <TabsTrigger key={f} value={f}>
                {LABEL[f]}
                {f !== "all" && counts[f] > 0 && (
                  <span className="font-mono text-2xs tabular-nums text-muted-foreground">
                    {counts[f]}
                  </span>
                )}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
        {controls}
        <Label className="ml-auto flex items-center gap-2 text-sm">
          <Switch
            checked={includeFiltered}
            onCheckedChange={(checked) => onIncludeFilteredChange(checked === true)}
            aria-label="show filtered deliveries"
          />
          Show filtered
        </Label>
      </div>

      {error ? (
        <EmptyState tone="error">Couldn’t load activity. {String(error)}</EmptyState>
      ) : isPending ? (
        <SkeletonRows rows={4} columns={["52px", "14px", "minmax(0,1fr)", "70px"]} />
      ) : rows.length === 0 ? (
        <EmptyState>
          {filter === "all" ? emptyText : `Nothing ${LABEL[filter].toLowerCase()}.`}
        </EmptyState>
      ) : (
        <ol className="ruled-paper flex flex-col gap-0.5" data-testid="activity-list">
          {rows.map((row) =>
            row.kind === "drop" ? (
              <DropEntry
                key={`d-${row.drop.automationId ?? ""}-${row.drop.droppedAt}-${row.drop.eventKey}`}
                drop={row.drop}
                now={now}
                automationName={row.drop.automationId ? nameOf?.(row.drop.automationId) : undefined}
              />
            ) : row.kind === "run" ? (
              <ActivityEntry
                key={row.run.id}
                run={row.run}
                now={now}
                automationName={nameOf?.(row.run.automationId)}
                agentLabel={
                  row.run.instanceId !== "" ? agentLabelOf?.(row.run.instanceId) : undefined
                }
                blockTypeOf={
                  blockTypeOf ? (blockId) => blockTypeOf(row.run.automationId, blockId) : undefined
                }
                defaultOpen={openRunId === row.run.id}
              />
            ) : (
              <li
                key={`w-${row.window.beforeRunId}-${row.window.firstAt}`}
                data-testid="filtered-window"
                className="flex h-8 items-center rounded-sm bg-secondary px-3 text-xs text-muted-foreground"
                title="Deliveries in this window did not pass the automation's filter."
              >
                {windowSummary([row.window], nameOf)}
              </li>
            ),
          )}
        </ol>
      )}
    </section>
  );
}

/** A delivery that did not fire: the same line shape as a run (age · dot ·
 * what came in / why it went nowhere), with no steps to unfold. */
function DropEntry({
  drop,
  now,
  automationName,
}: {
  drop: ActivityDrop;
  now: number;
  automationName?: string;
}) {
  return (
    <li
      data-testid="activity-drop"
      className="grid grid-cols-[52px_14px_minmax(0,1fr)_70px] items-start gap-x-3 rounded-md px-3 py-2 opacity-65"
      title="This event was delivered but no run started."
    >
      <span className="pt-0.5 font-mono text-2xs tabular-nums text-muted-foreground">
        {relativeAge(drop.droppedAt, now)}
      </span>
      <span className="pt-1">
        <StatusDot tone="caution" size={8} label="status did not fire" />
      </span>
      <span className="min-w-0">
        <span className="block truncate text-sm font-medium">
          Did not fire · {drop.eventKey.replaceAll("_", " ").replaceAll(".", " · ")}
          {automationName && (
            <span className="ml-2 font-normal text-muted-foreground">{automationName}</span>
          )}
        </span>
        <span className="block truncate text-xs text-muted-foreground">
          {dropReasonText(drop.reason)}
          {drop.detail && ` · ${drop.detail}`}
        </span>
      </span>
    </li>
  );
}
