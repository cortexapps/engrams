import { useState } from "react";
import { Link } from "@tanstack/react-router";

import { StatusDot, type StatusTone } from "@/components/status-dot";
import type {
  AutomationInstance,
  AutomationRunBrief,
  AutomationSummary,
} from "@/gen/engram/app/v1/automation_pb";
import { useRunList } from "@/hooks/useAutomationRuns";
import { useInstance } from "@/hooks/useInstances";
import { relativeAge, relativeTime } from "@/lib/relative-time";
import { isActiveRunStatus, runStatusLabel } from "@/pages/automations/runs/run-format";

import { HandleChip } from "./HandleChip";

export function humanizeWorkstreamName(key: string): string {
  return key.replace(/[-_]+/g, " ");
}

/** How many places a row shows before it folds the rest behind "+N more". */
export const VISIBLE_HANDLES = 3;

function HandleCell({ handles, pending }: { handles: string[]; pending: boolean }) {
  const [showAll, setShowAll] = useState(false);
  const shown = showAll ? handles : handles.slice(0, VISIBLE_HANDLES);
  const hidden = handles.length - shown.length;
  return (
    <div role="cell" className="flex min-w-0 flex-wrap items-center gap-1.5">
      {shown.map((handle) => (
        <HandleChip key={handle} handle={handle} />
      ))}
      {(hidden > 0 || showAll) && handles.length > VISIBLE_HANDLES && (
        <button
          type="button"
          className="rounded-sm px-1 text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
          onClick={() => setShowAll((v) => !v)}
        >
          {showAll ? "Show fewer" : `+${hidden} more`}
        </button>
      )}
      {!pending && handles.length === 0 && (
        <span className="text-xs text-muted-foreground">No linked place</span>
      )}
    </div>
  );
}

function workstreamTone(instance: AutomationInstance, latest?: AutomationRunBrief): StatusTone {
  if (instance.status === "closed") return "muted";
  if (latest?.status === "waiting") return "caution";
  if (latest && isActiveRunStatus(latest.status)) return "active";
  return "nominal";
}

const WITH_AUTOMATION = "grid-cols-[minmax(0,1.5fr)_150px_minmax(0,1.3fr)_130px_90px]";
const WITHOUT_AUTOMATION = "grid-cols-[minmax(0,1.5fr)_minmax(0,1.3fr)_130px_90px]";

export function WorkstreamsTable({
  instances,
  automations,
  showAutomation,
  now,
}: {
  instances: readonly AutomationInstance[];
  automations: readonly AutomationSummary[];
  showAutomation: boolean;
  now: number;
}) {
  const columns = showAutomation ? WITH_AUTOMATION : WITHOUT_AUTOMATION;
  return (
    <div className="overflow-x-auto rounded-lg border bg-card" role="table">
      <div className={showAutomation ? "min-w-[920px]" : "min-w-[720px]"}>
        <div
          role="row"
          className={`grid ${columns} gap-4 border-b px-4 py-2.5 text-xs font-semibold text-muted-foreground`}
        >
          <span role="columnheader">Workstream</span>
          {showAutomation && <span role="columnheader">Automation</span>}
          <span role="columnheader">Where it lives</span>
          <span role="columnheader">Last run</span>
          <span role="columnheader">Opened</span>
        </div>
        <div className="divide-y">
          {instances.map((instance) => (
            <WorkstreamRow
              key={instance.id}
              instance={instance}
              automationName={
                automations.find((summary) => summary.automation?.id === instance.automationId)
                  ?.automation?.name
              }
              showAutomation={showAutomation}
              columns={columns}
              now={now}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

function WorkstreamRow({
  instance,
  automationName,
  showAutomation,
  columns,
  now,
}: {
  instance: AutomationInstance;
  automationName?: string;
  showAutomation: boolean;
  columns: string;
  now: number;
}) {
  const detail = useInstance(instance.id);
  const runs = useRunList(instance.automationId, { instanceId: instance.id, limit: 1 });
  const latest = runs.data?.runs[0];
  const tone = workstreamTone(instance, latest);

  return (
    <div
      role="row"
      data-testid="workstream-row"
      className={`grid ${columns} items-center gap-4 px-4 py-3.5 text-sm`}
    >
      <div role="cell" className="flex min-w-0 items-start gap-2.5">
        <StatusDot tone={tone} size={8} label={instance.status} className="mt-1.5" />
        <div className="min-w-0">
          <Link
            to="/automations/workstreams/$id"
            params={{ id: instance.id }}
            className="block truncate font-semibold underline-offset-4 hover:underline"
          >
            {humanizeWorkstreamName(instance.key)}
          </Link>
          <span className="block truncate font-mono text-2xs text-muted-foreground">
            {instance.key}
          </span>
        </div>
      </div>
      {showAutomation && (
        <span role="cell" className="truncate">
          {automationName ?? "Unknown automation"}
        </span>
      )}
      <HandleCell
        handles={detail.data?.handles.map((h) => h.handle) ?? []}
        pending={detail.isPending}
      />
      <div role="cell" className="min-w-0 text-xs">
        {latest ? (
          <span className="flex items-center gap-1.5">
            <span className="truncate">{runStatusLabel(latest.status)}</span>
            <span className="shrink-0 font-mono text-2xs tabular-nums text-muted-foreground">
              {relativeTime(latest.createdAt, now)}
            </span>
          </span>
        ) : runs.isPending ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span className="text-muted-foreground">No activity</span>
        )}
      </div>
      <span role="cell" className="font-mono text-xs tabular-nums text-muted-foreground">
        {relativeAge(instance.openedAt, now)}
      </span>
    </div>
  );
}
