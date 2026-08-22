import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { ChevronRightIcon } from "lucide-react";

import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";
import { useRunList } from "@/hooks/useAutomationRuns";
import { relativeTime } from "@/pages/sessions/session-format";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { runStatusTone, toneDotClass } from "@/lib/automations";
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
    <span
      role="img"
      aria-label={`status ${runStatusLabel(status)}`}
      data-tone={runStatusTone(status)}
      className={cn(
        "inline-block size-2 shrink-0 rounded-full",
        toneDotClass(runStatusTone(status)),
        className,
      )}
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
        <span className="ml-auto tabular-nums">{relativeTime(window.lastAt, now)} ago</span>
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

function RunRow({ run, now }: { run: AutomationRunBrief; now: number }) {
  return (
    <li>
      <Link
        to="/settings/automations/$id/runs/$runId"
        params={{ id: run.automationId, runId: run.id }}
        className="flex items-center gap-3 rounded-md px-3 py-2 text-sm hover:bg-secondary/60"
        data-testid="run-row"
      >
        <RunStatusDot status={run.status} />
        <span className="w-24 shrink-0 capitalize">{runStatusLabel(run.status)}</span>
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          {triggerSourceLabel(run.triggerSource, run.eventKey)}
          {run.dryRun && <span className="ml-2 rounded bg-secondary px-1.5 text-xs">dry run</span>}
        </span>
        <span className="w-20 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {formatDuration(run.startedAt, run.endedAt, now)}
        </span>
        <span className="w-16 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {relativeTime(run.createdAt, now)} ago
        </span>
      </Link>
    </li>
  );
}

export function RunsTab({ automationId, now = Date.now }: RunsTabProps) {
  const [includeFiltered, setIncludeFiltered] = useState(false);
  const runs = useRunList(automationId, { includeFiltered });
  const tick = now();

  const rows = interleaveRuns(runs.data?.runs ?? [], runs.data?.filtered ?? []);

  return (
    <section className="flex flex-col gap-3" aria-label="Runs">
      <div className="flex items-center justify-between">
        <p className="text-sm text-muted-foreground">
          {runs.data ? `${runs.data.runs.length} runs` : "Loading runs…"}
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
      {runs.data && rows.length === 0 && (
        <p className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
          No runs yet. The next trigger occurrence lands here.
        </p>
      )}
      <ul className="flex flex-col gap-1">
        {rows.map((row) =>
          row.kind === "run" ? (
            <RunRow key={row.run.id} run={row.run} now={tick} />
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
