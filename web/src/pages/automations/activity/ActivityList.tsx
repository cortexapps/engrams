import { useState, type ReactNode } from "react";

import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

import { ActivityEntry } from "./ActivityEntry";
import {
  filterCounts,
  interleaveRuns,
  matchesFilter,
  windowSummary,
  type ActivityFilter,
} from "./activity-format";

// The activity ledger shared by the per-automation tab, the cross-automation
// page, and a workstream's timeline: pill filters, the "show filtered" switch,
// and one entry per run with the filtered deliveries folded into a quiet row.

export interface ActivityListProps {
  runs: readonly AutomationRunBrief[];
  windows: readonly (FilteredWindow & { automationId?: string })[];
  isPending: boolean;
  error?: unknown;
  now: number;
  includeFiltered: boolean;
  onIncludeFilteredChange: (next: boolean) => void;
  /** Cross-automation: name each entry's automation. */
  nameOf?: (automationId: string) => string | undefined;
  /** Workstream names by instance id, for the chip on a bound run. */
  workstreamLabelOf?: (instanceId: string) => string | undefined;
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
  isPending,
  error,
  now,
  includeFiltered,
  onIncludeFilteredChange,
  nameOf,
  workstreamLabelOf,
  superseded = false,
  controls,
  openRunId,
  emptyText = "No activity yet. The next trigger occurrence lands here.",
}: ActivityListProps) {
  const [filter, setFilter] = useState<ActivityFilter>("all");
  const counts = filterCounts(runs);
  const shown = runs.filter((r) => matchesFilter(r, filter));
  const rows = interleaveRuns(shown, filter === "all" ? windows : []);
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
        <ol className="flex flex-col gap-0.5" data-testid="activity-list">
          {rows.map((row) =>
            row.kind === "run" ? (
              <ActivityEntry
                key={row.run.id}
                run={row.run}
                now={now}
                automationName={nameOf?.(row.run.automationId)}
                workstreamLabel={
                  row.run.instanceId !== "" ? workstreamLabelOf?.(row.run.instanceId) : undefined
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
