import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { ChevronRightIcon } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";
import { useRunList } from "@/hooks/useAutomationRuns";
import { useInstanceList } from "@/hooks/useInstances";
import { relativeTime } from "@/lib/relative-time";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { runStatusTone } from "@/lib/automations";
import { cn } from "@/lib/utils";
import { formatDuration, runStatusLabel, triggerSourceLabel } from "./run-format";

export interface RunsTabProps {
  automationId: string;
  /** Injected by tests; defaults to the wall clock. */
  now?: () => number;
}

/** Status dot: form carries the state, so a scan needs no reading. */
export function RunStatusDot({ status, className }: { status: string; className?: string }) {
  return (
    <StatusDot
      tone={runStatusTone(status)}
      label={`status ${runStatusLabel(status)}`}
      className={className}
    />
  );
}

type Row = { kind: "run"; run: AutomationRunBrief } | { kind: "window"; window: FilteredWindow };

/** Interleave runs (newest first) with the filtered windows that precede each
 * run; `before_run_id === ""` pins a window after the oldest run. */
export function interleaveRuns(
  runs: readonly AutomationRunBrief[],
  windows: readonly FilteredWindow[],
): Row[] {
  const byBefore = new Map<string, FilteredWindow[]>();
  for (const window of windows) {
    const list = byBefore.get(window.beforeRunId) ?? [];
    list.push(window);
    byBefore.set(window.beforeRunId, list);
  }
  const rows: Row[] = [];
  for (const run of runs) {
    for (const window of byBefore.get(run.id) ?? []) rows.push({ kind: "window", window });
    rows.push({ kind: "run", run });
  }
  for (const window of byBefore.get("") ?? []) rows.push({ kind: "window", window });
  return rows;
}

function WindowRow({ window, now }: { window: FilteredWindow; now: number }) {
  const [open, setOpen] = useState(false);
  return (
    <li className="rounded-md border border-dashed border-border/70 text-xs text-muted-foreground">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-secondary/60"
      >
        <ChevronRightIcon className={cn("size-3 transition-transform", open && "rotate-90")} />
        <span>
          {window.count} filtered {window.count === 1 ? "event" : "events"}
        </span>
        <span className="ml-auto tabular-nums">{relativeTime(window.lastAt, now)}</span>
      </button>
      {open && (
        <p className="px-3 pb-2 pl-8">
          Deliveries between {new Date(window.firstAt).toLocaleString()} and{" "}
          {new Date(window.lastAt).toLocaleString()} did not pass the automation's filter. Turn on
          "show filtered" to list them.
        </p>
      )}
    </li>
  );
}

function RunRow({
  run,
  now,
  workstreamLabel,
}: {
  run: AutomationRunBrief;
  now: number;
  /** ADR 0120: the owning workstream's rendered key ("" hides the chip —
   * either an unbound run, or the per-workstream list where it is
   * redundant). */
  workstreamLabel?: string;
}) {
  return (
    <li>
      <Link
        to="/automations/$id/runs/$runId"
        params={{ id: run.automationId, runId: run.id }}
        className="flex items-center gap-3 rounded-md px-3 py-2 text-sm hover:bg-secondary/60"
        data-testid="run-row"
      >
        <RunStatusDot status={run.status} />
        <span className="w-24 shrink-0 capitalize">{runStatusLabel(run.status)}</span>
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          {triggerSourceLabel(run.triggerSource, run.eventKey)}
          {run.dryRun && <span className="ml-2 rounded bg-secondary px-1.5 text-xs">dry run</span>}
          {workstreamLabel !== undefined && workstreamLabel !== "" && (
            <span
              className="ml-2 rounded bg-secondary px-1.5 text-xs"
              data-testid="run-workstream-chip"
            >
              {workstreamLabel}
            </span>
          )}
        </span>
        <span className="w-20 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {formatDuration(run.startedAt, run.endedAt, now)}
        </span>
        <span className="w-16 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {relativeTime(run.createdAt, now)}
        </span>
      </Link>
    </li>
  );
}

/** The bare run list — reused by the Runs tab and by a workstream's
 * detail panel (which passes `instanceId` and hides the redundant chip). */
export function RunList({
  automationId,
  instanceId,
  now = Date.now,
}: {
  automationId: string;
  instanceId?: string;
  now?: () => number;
}) {
  const runs = useRunList(automationId, {
    includeFiltered: true,
    ...(instanceId !== undefined ? { instanceId } : {}),
  });
  const tick = now();
  if (!runs.data) return null;
  if (runs.data.runs.length === 0) {
    return <EmptyState inline>No runs yet.</EmptyState>;
  }
  return (
    <ul className="flex flex-col gap-1" data-testid="workstream-runs">
      {runs.data.runs.map((run) => (
        <RunRow key={run.id} run={run} now={tick} />
      ))}
    </ul>
  );
}

export function RunsTab({ automationId, now = Date.now }: RunsTabProps) {
  const [includeFiltered, setIncludeFiltered] = useState(false);
  const runs = useRunList(automationId, { includeFiltered });
  const tick = now();

  const rows = interleaveRuns(runs.data?.runs ?? [], runs.data?.filtered ?? []);
  // ADR 0120: label instance-bound runs by their workstream's rendered key.
  // The list is only fetched once a bound run is actually visible.
  const anyBound = (runs.data?.runs ?? []).some((run) => run.instanceId !== "");
  const instances = useInstanceList(automationId, {
    includeClosed: true,
    enabled: anyBound,
  });
  const keyById = new Map(
    (instances.data?.instances ?? []).map((instance) => [instance.id, instance.key]),
  );

  return (
    <section className="flex flex-col gap-3" aria-label="Runs">
      <div className="flex items-center justify-between">
        <p className="text-sm text-muted-foreground">
          {runs.data ? `${runs.data.runs.length} runs` : ""}
        </p>
        <Label className="flex items-center gap-2 text-sm">
          <Switch
            checked={includeFiltered}
            onCheckedChange={(checked) => setIncludeFiltered(checked === true)}
            aria-label="show filtered runs"
          />
          Show filtered
        </Label>
      </div>
      {!runs.data && <SkeletonRows columns={["8px", "96px", "minmax(0,1fr)", "80px", "64px"]} />}
      {runs.data && rows.length === 0 && (
        <EmptyState>No runs yet. The next trigger occurrence lands here.</EmptyState>
      )}
      <ul className="flex flex-col gap-1">
        {rows.map((row) =>
          row.kind === "run" ? (
            <RunRow
              key={row.run.id}
              run={row.run}
              now={tick}
              workstreamLabel={
                row.run.instanceId !== ""
                  ? (keyById.get(row.run.instanceId) ?? row.run.instanceId)
                  : ""
              }
            />
          ) : (
            <WindowRow
              key={`window:${row.window.beforeRunId}:${row.window.firstAt}`}
              window={row.window}
              now={tick}
            />
          ),
        )}
      </ul>
    </section>
  );
}
