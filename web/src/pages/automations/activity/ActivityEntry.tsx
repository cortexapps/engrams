import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import { ChevronRightIcon } from "lucide-react";
import { toast } from "sonner";

import type { AutomationRunBrief } from "@/gen/engram/app/v1/automation_pb";
import { useRetryRun, useRun, useStopRun } from "@/hooks/useAutomationRuns";
import { errorMessage } from "@/lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { runStatusTone } from "@/lib/automations";
import { stepSentence } from "@/lib/step-sentence";
import { Button } from "@/components/ui/button";
import { SkeletonRows } from "@/components/skeleton-rows";
import { cn } from "@/lib/utils";

import { RunStatusDot } from "../runs/RunStatusDot";
import { StepRunDrawer } from "../runs/StepRunDrawer";
import {
  buildTimeline,
  findTimelineStep,
  formatDuration,
  isActiveRunStatus,
  parseJsonObject,
  type TimelineNode,
  type TimelineStep,
} from "../runs/run-format";
import { ACTIVE, entryTitle, entryVia, filteredReason, pointsAtFleet } from "./activity-format";

// One activity entry = one run. Collapsed, it is a single line a person can
// scan: age · dot · what happened / how it got in · how long it took, and the
// reason when it failed. Expanded, the step trace unfolds inline (no page
// hop): one row per step that says what the block did, in words; the failed
// step tinted with its error verbatim; a click on a row opens the details
// (rendered inputs, outputs, stdout) in a drawer; then the actions that fit
// the outcome — retry, stop, the session it opened, and a related cause when
// the error points at the fleet.

export interface ActivityEntryProps {
  run: AutomationRunBrief;
  now: number;
  /** Cross-automation ledger: name the automation on the row. */
  automationName?: string;
  /** The agent's rendered name, when the run is bound to one. */
  agentLabel?: string;
  /** The kind of a block in this run's automation, for the step sentences. */
  blockTypeOf?: (blockId: string) => string | undefined;
  defaultOpen?: boolean;
}

const QUIET = new Set(["filtered", "superseded", "halted", "skipped"]);

function firstLine(text: string, max = 120): string {
  const line = text.split("\n")[0] ?? "";
  return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

export function ActivityEntry({
  run,
  now,
  automationName,
  agentLabel,
  blockTypeOf,
  defaultOpen = false,
}: ActivityEntryProps) {
  const [open, setOpen] = useState(defaultOpen);
  const tone = runStatusTone(run.status);
  const active = ACTIVE.has(run.status);
  const filtered = filteredReason(run);
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
            {agentLabel && ` · ${agentLabel}`}
            {run.dryRun && " · dry run"}
            {run.startedAt && (
              <>
                {" · "}
                <span className="font-mono tabular-nums">
                  {formatDuration(run.startedAt, run.endedAt, now)}
                </span>
              </>
            )}
          </span>
          {tone === "critical" && run.error && (
            <span className="block truncate text-xs text-foreground" data-testid="entry-reason">
              {firstLine(run.error)}
            </span>
          )}
          {filtered && (
            <span className="block truncate text-xs" data-testid="entry-reason">
              {filtered}
            </span>
          )}
        </span>
        <span className="inline-flex items-center justify-end gap-1 pt-0.5 text-xs text-muted-foreground">
          steps
          <ChevronRightIcon
            className={cn("size-3 transition-transform duration-150", open && "rotate-90")}
            aria-hidden
          />
        </span>
      </button>
      {open && <InlineTrace run={run} now={now} blockTypeOf={blockTypeOf} />}
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

function InlineTrace({
  run,
  now,
  blockTypeOf,
}: {
  run: AutomationRunBrief;
  now: number;
  blockTypeOf?: (blockId: string) => string | undefined;
}) {
  const query = useRun(run.id);
  const stop = useStopRun();
  const retry = useRetryRun();
  const steps = query.data?.run?.steps ?? [];
  const sessionIds = query.data?.run?.sessionIds ?? [];
  const tree = useMemo(() => buildTimeline(steps), [steps]);
  const rows = flatten(tree);
  const active = isActiveRunStatus(run.status);
  const failedStep = steps.find((s) => s.status === "failed" || s.status === "deadline");
  const fleetCause = pointsAtFleet(run.error ?? failedStep?.error);
  // The details drawer follows a step by its path, so it tracks each poll.
  const [detailsPath, setDetailsPath] = useState<string | null>(null);
  const details = detailsPath === null ? null : findTimelineStep(tree, detailsPath);

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
              <StepRow
                key={row.path}
                step={row}
                blockType={blockTypeOf?.(row.blockId)}
                now={now}
                onDetails={() => setDetailsPath(row.path)}
              />
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
        <Button asChild variant="ghost" size="sm">
          <Link
            to="/automations/$id"
            params={{ id: run.automationId }}
            search={{ tab: "build", run: run.id }}
            data-testid="replay-link"
          >
            Replay on canvas
          </Link>
        </Button>
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
      <StepRunDrawer
        step={details}
        blockType={details ? blockTypeOf?.(details.blockId) : undefined}
        onClose={() => setDetailsPath(null)}
      />
    </div>
  );
}

function StepRow({
  step,
  blockType,
  now,
  onDetails,
}: {
  step: TimelineStep;
  blockType: string | undefined;
  now: number;
  onDetails: () => void;
}) {
  const { latest } = step;
  const failed = latest.status === "failed" || latest.status === "deadline";
  const skipped = latest.status === "skipped";
  const sentence = stepSentence(blockType, {
    status: latest.status,
    ...(latest.error !== undefined ? { error: latest.error } : {}),
    inputs: parseJsonObject(latest.inputsJson) ?? {},
    outputs: parseJsonObject(latest.outputsJson) ?? {},
  });
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
      className={cn("rounded-[6px]", skipped && "opacity-50")}
    >
      <button
        type="button"
        onClick={onDetails}
        aria-label={`Details for ${step.blockId}`}
        className="grid min-h-7 w-full grid-cols-[14px_160px_minmax(0,1fr)_70px] items-start gap-x-3 rounded-[6px] px-2 py-1 text-left hover:bg-accent/60"
      >
        <span className="pt-1">
          <RunStatusDot status={latest.status} size={6} />
        </span>
        <span className="truncate font-mono text-xs">{step.blockId}</span>
        <span className={cn("text-xs", failed ? "text-foreground" : "text-muted-foreground")}>
          {sentence}
          {step.attempts.length > 1 && (
            <span className="ml-2 font-mono text-2xs text-muted-foreground">
              {step.attempts.length} attempts
            </span>
          )}
        </span>
        <span className="text-right font-mono text-2xs tabular-nums text-muted-foreground">
          {formatDuration(latest.startedAt, latest.endedAt, now)}
        </span>
      </button>
    </li>
  );
}
