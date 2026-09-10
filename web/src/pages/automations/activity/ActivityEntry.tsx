import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { ChevronRightIcon } from "lucide-react";
import { toast } from "sonner";

import type { AutomationRunBrief, AutomationStepRun } from "@/gen/engram/app/v1/automation_pb";
import { useRetryRun, useRun, useStopRun } from "@/hooks/useAutomationRuns";
import { errorMessage } from "@/lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { runStatusTone } from "@/lib/automations";
import { Button } from "@/components/ui/button";
import { SkeletonRows } from "@/components/skeleton-rows";
import { cn } from "@/lib/utils";

import { RunStatusDot } from "../runs/RunStatusDot";
import {
  buildTimeline,
  formatDuration,
  isActiveRunStatus,
  parseJsonObject,
  runStatusLabel,
  type TimelineNode,
  type TimelineStep,
} from "../runs/run-format";
import { ACTIVE, entryTitle, entryVia, pointsAtFleet } from "./activity-format";

// One activity entry = one run. Collapsed, it is a single line a person can
// scan: age · dot · what happened / how it got in · how long it took. Expanded,
// the step trace unfolds inline (no page hop): one 28px row per step with the
// failed step tinted and its error verbatim, then the actions that fit the
// outcome — retry, stop, the session it opened, and a related cause when the
// error points at the fleet.

export interface ActivityEntryProps {
  run: AutomationRunBrief;
  now: number;
  /** Cross-automation ledger: name the automation on the row. */
  automationName?: string;
  /** The workstream's rendered name, when the run is bound to one. */
  workstreamLabel?: string;
  defaultOpen?: boolean;
}

const QUIET = new Set(["filtered", "superseded", "halted", "skipped"]);

export function ActivityEntry({
  run,
  now,
  automationName,
  workstreamLabel,
  defaultOpen = false,
}: ActivityEntryProps) {
  const [open, setOpen] = useState(defaultOpen);
  const tone = runStatusTone(run.status);
  const active = ACTIVE.has(run.status);
  return (
    <li
      data-testid="activity-entry"
      data-status={run.status}
      className={cn("rounded-md", QUIET.has(run.status) && "opacity-65", active && "row-running")}
      style={
        tone === "critical"
          ? {
              backgroundColor:
                "color-mix(in oklch, var(--color-instrument-critical) 5%, transparent)",
            }
          : active
            ? { backgroundColor: "color-mix(in oklch, var(--color-ring) 6%, transparent)" }
            : undefined
      }
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="grid w-full grid-cols-[52px_14px_minmax(0,1fr)_70px] items-start gap-x-3 rounded-md px-3 py-2 text-left hover:bg-accent/60"
      >
        <span className="pt-0.5 font-mono text-2xs tabular-nums text-muted-foreground">
          {relativeAge(run.createdAt, now)}
        </span>
        <span className="pt-1">
          <RunStatusDot status={run.status} />
        </span>
        <span className="min-w-0">
          <span className="block truncate text-sm font-medium">
            {entryTitle(run)}
            {automationName && (
              <span className="ml-2 font-normal text-muted-foreground">{automationName}</span>
            )}
          </span>
          <span className="block truncate text-xs text-muted-foreground">
            {entryVia(run)}
            {workstreamLabel && ` · ${workstreamLabel}`}
            {run.dryRun && " · dry run"}
            {" · "}
            <span className="font-mono tabular-nums">
              {formatDuration(run.startedAt, run.endedAt, now)}
            </span>
          </span>
        </span>
        <span className="inline-flex items-center justify-end gap-1 pt-0.5 text-xs text-muted-foreground">
          steps
          <ChevronRightIcon
            className={cn("size-3 transition-transform duration-150", open && "rotate-90")}
            aria-hidden
          />
        </span>
      </button>
      {open && <InlineTrace run={run} now={now} />}
    </li>
  );
}

/** Flatten the timeline tree: iteration groups become a labelled row, their
 * steps indent one level. */
function flatten(nodes: readonly TimelineNode[], out: (TimelineStep | string)[] = []) {
  for (const node of nodes) {
    if (node.kind === "step") out.push(node.step);
    else {
      out.push(`${node.loopId} · iteration ${node.iteration + 1}`);
      flatten(node.steps, out);
    }
  }
  return out;
}

/** The step's one-line outcome: the error verbatim, else a compact readout of
 * its outputs, else the status word. */
function outcomeText(step: AutomationStepRun): string {
  if (step.error) return step.error;
  const outputs = parseJsonObject(step.outputsJson);
  if (outputs) {
    const parts = Object.entries(outputs)
      .filter(([, v]) => typeof v === "string" || typeof v === "number" || typeof v === "boolean")
      .slice(0, 3)
      .map(([k, v]) => `${k} ${String(v).slice(0, 40)}`);
    if (parts.length > 0) return parts.join(" · ");
  }
  return runStatusLabel(step.status);
}

function InlineTrace({ run, now }: { run: AutomationRunBrief; now: number }) {
  const query = useRun(run.id);
  const stop = useStopRun();
  const retry = useRetryRun();
  const steps = query.data?.run?.steps ?? [];
  const sessionIds = query.data?.run?.sessionIds ?? [];
  const rows = flatten(buildTimeline(steps));
  const active = isActiveRunStatus(run.status);
  const failedStep = steps.find((s) => s.status === "failed" || s.status === "deadline");
  const fleetCause = pointsAtFleet(run.error ?? failedStep?.error);

  const onStop = async () => {
    try {
      await stop.mutateAsync({ runId: run.id, reason: "stopped from activity" });
      toast.success("Stop requested");
    } catch (e) {
      toast.error(errorMessage(e));
    }
  };
  const onRetry = async () => {
    try {
      await retry.mutateAsync({ runId: run.id });
      toast.success(run.status === "completed" ? "Running again" : "Retry started");
    } catch (e) {
      toast.error(errorMessage(e));
    }
  };

  return (
    <div className="pb-3 pl-[76px] pr-3" data-testid="inline-trace">
      {query.isPending ? (
        <SkeletonRows rows={3} columns={["14px", "160px", "minmax(0,1fr)", "70px"]} />
      ) : rows.length === 0 ? (
        <p className="py-2 text-xs text-muted-foreground">No steps recorded yet.</p>
      ) : (
        <ol className="flex flex-col gap-px" aria-label="Step trace">
          {rows.map((row, i) =>
            typeof row === "string" ? (
              <li key={i} className="px-2 pt-2 pb-0.5 text-2xs font-semibold text-muted-foreground">
                {row}
              </li>
            ) : (
              <StepRow key={row.path} step={row} now={now} />
            ),
          )}
        </ol>
      )}
      {run.error && !failedStep && (
        <p className="mt-2 text-xs text-foreground" role="alert">
          {run.error}
        </p>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-2">
        {active ? (
          <Button
            variant="outline"
            size="sm"
            onClick={() => void onStop()}
            disabled={stop.isPending}
          >
            Stop
          </Button>
        ) : (
          <Button
            variant="outline"
            size="sm"
            onClick={() => void onRetry()}
            disabled={retry.isPending}
          >
            {run.status === "completed" ? "Run again" : "Retry"}
          </Button>
        )}
        {sessionIds.map((id) => (
          <Button key={id} asChild variant="ghost" size="sm">
            <Link to="/sessions/$id" params={{ id }}>
              Open session ↗
            </Link>
          </Button>
        ))}
        {fleetCause && (
          <Button asChild variant="ghost" size="sm">
            <Link to="/settings/fleet">Related cause · Settings › Fleet</Link>
          </Button>
        )}
      </div>
    </div>
  );
}

function StepRow({ step, now }: { step: TimelineStep; now: number }) {
  const { latest } = step;
  const failed = latest.status === "failed" || latest.status === "deadline";
  const skipped = latest.status === "skipped";
  return (
    <li
      data-testid="trace-step"
      data-status={latest.status}
      style={{
        paddingLeft: step.depth * 12,
        backgroundColor: failed
          ? "color-mix(in oklch, var(--color-instrument-critical) 8%, transparent)"
          : undefined,
      }}
      className={cn(
        "grid min-h-7 grid-cols-[14px_160px_minmax(0,1fr)_70px] items-start gap-x-3 rounded-[6px] px-2 py-1",
        skipped && "opacity-50",
      )}
    >
      <span className="pt-1">
        <RunStatusDot status={latest.status} size={6} />
      </span>
      <span className="truncate font-mono text-xs">{step.blockId}</span>
      <span className={cn("text-xs", failed ? "text-foreground" : "text-muted-foreground")}>
        {outcomeText(latest)}
        {step.attempts.length > 1 && (
          <span className="ml-2 font-mono text-2xs text-muted-foreground">
            {step.attempts.length} attempts
          </span>
        )}
      </span>
      <span className="text-right font-mono text-2xs tabular-nums text-muted-foreground">
        {formatDuration(latest.startedAt, latest.endedAt, now)}
      </span>
    </li>
  );
}
