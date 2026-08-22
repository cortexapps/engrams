import { useState } from "react";
import { ChevronRightIcon } from "lucide-react";

import type { AutomationStepRun } from "@/gen/engram/app/v1/automation_pb";
import { cn } from "@/lib/utils";
import { RunStatusDot } from "./RunsTab";
import {
  buildTimeline,
  formatDuration,
  runStatusLabel,
  type TimelineGroup,
  type TimelineNode,
  type TimelineStep,
} from "./run-format";

export interface RunTimelineProps {
  steps: readonly AutomationStepRun[];
  /** Block type by block id, from the pinned definition; optional. */
  blockTypes?: Record<string, string>;
  selectedPath?: string;
  onSelect(step: TimelineStep): void;
  now?: number;
}

const INDENT_PX = 16;

function StepRow({
  step,
  blockType,
  selected,
  onSelect,
  now,
}: {
  step: TimelineStep;
  blockType?: string;
  selected: boolean;
  onSelect(step: TimelineStep): void;
  now: number;
}) {
  const { latest } = step;
  return (
    <li style={{ paddingLeft: step.depth * INDENT_PX }} data-depth={step.depth}>
      <button
        type="button"
        onClick={() => onSelect(step)}
        aria-pressed={selected}
        data-testid="timeline-step"
        className={cn(
          "flex w-full items-center gap-3 rounded-md px-3 py-1.5 text-left text-sm hover:bg-secondary/60",
          selected && "bg-secondary ring-1 ring-border",
        )}
      >
        <RunStatusDot status={latest.status} />
        <span className="font-medium">{step.blockId}</span>
        {blockType && <span className="text-xs text-muted-foreground">{blockType}</span>}
        {step.attempt > 0 && (
          <span className="rounded bg-secondary px-1.5 text-xs tabular-nums" data-testid="attempt">
            attempt {step.attempt + 1}
          </span>
        )}
        <span className="ml-auto text-xs capitalize text-muted-foreground">
          {runStatusLabel(latest.status)}
        </span>
        <span className="w-16 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
          {formatDuration(latest.startedAt, latest.endedAt, now)}
        </span>
      </button>
    </li>
  );
}

function IterationGroup({
  group,
  render,
}: {
  group: TimelineGroup;
  render(nodes: TimelineNode[]): React.ReactNode;
}) {
  const [open, setOpen] = useState(true);
  return (
    <li style={{ paddingLeft: group.depth * INDENT_PX }} data-testid="iteration-group">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-1 text-left text-xs font-semibold uppercase tracking-wide text-muted-foreground"
      >
        <ChevronRightIcon className={cn("size-3 transition-transform", open && "rotate-90")} />
        {group.loopId} · iteration {group.iteration + 1}
      </button>
      {open && <ul className="flex flex-col gap-0.5">{render(group.steps)}</ul>}
    </li>
  );
}

export function RunTimeline({
  steps,
  blockTypes,
  selectedPath,
  onSelect,
  now = Date.now(),
}: RunTimelineProps) {
  const nodes = buildTimeline(steps);

  const render = (list: TimelineNode[]): React.ReactNode =>
    list.map((node) =>
      node.kind === "step" ? (
        <StepRow
          key={node.step.path}
          step={node.step}
          blockType={blockTypes?.[node.step.blockId]}
          selected={node.step.path === selectedPath}
          onSelect={onSelect}
          now={now}
        />
      ) : (
        <IterationGroup key={`${node.loopId}[${node.iteration}]`} group={node} render={render} />
      ),
    );

  if (nodes.length === 0) {
    return (
      <p className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
        No steps recorded yet.
      </p>
    );
  }
  return (
    <ol className="flex flex-col gap-0.5" aria-label="Step timeline">
      {render(nodes)}
    </ol>
  );
}
