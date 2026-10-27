import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";

import { StatusDot, type StatusTone } from "@/components/status-dot";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import type {
  AutomationInstance,
  AutomationRunBrief,
  AutomationSummary,
} from "@/gen/engram/app/v1/automation_pb";
import { useRunList } from "@/hooks/useAutomationRuns";
import { useCloseInstance, useInstance } from "@/hooks/useInstances";
import { errorMessage } from "@/lib/errors";
import { relativeAge, relativeTime } from "@/lib/relative-time";
import { isActiveRunStatus, runStatusLabel } from "@/pages/automations/runs/run-format";

import { HandleChip } from "./HandleChip";
import { agentTitle } from "./handles";

// One row per scoped agent: what it is called (the place it owns, by
// default), where it lives, its last run, when it started, and Close while
// it is open. A product page passes its own naming (the Slack page names the
// channel).

/** What a row says about an agent: its title and a subtitle. */
export type DescribeAgent = (
  instance: AutomationInstance,
  handles: readonly string[],
) => { title: string; subtitle: string };

export const describeAgent: DescribeAgent = (instance, handles) => ({
  title: agentTitle(instance, handles),
  subtitle: instance.key,
});

/** A place's chip text; undefined = the generic parse. Chips with the same
 * text collapse into one (a thread's root and a reply in it are one place). */
export type HandleLabel = (handle: string) => string | undefined;

/** How many places a row shows before it folds the rest behind "+N more". */
export const VISIBLE_HANDLES = 3;

function HandleCell({
  handles: raw,
  pending,
  handleLabel,
}: {
  handles: readonly string[];
  pending: boolean;
  handleLabel?: HandleLabel;
}) {
  const [showAll, setShowAll] = useState(false);
  // One chip per distinct text: the brain's "Started a session" post binds
  // both the thread and its own reply, which are one place to a reader.
  const seen = new Set<string>();
  const handles = raw.flatMap((handle) => {
    const label = handleLabel?.(handle);
    const text = label ?? handle;
    if (seen.has(text)) return [];
    seen.add(text);
    return [{ handle, label }];
  });
  const shown = showAll ? handles : handles.slice(0, VISIBLE_HANDLES);
  const hidden = handles.length - shown.length;
  return (
    <div role="cell" className="flex min-w-0 flex-wrap items-center gap-1.5">
      {shown.map(({ handle, label }) => (
        <HandleChip key={handle} handle={handle} {...(label !== undefined ? { label } : {})} />
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

function agentTone(instance: AutomationInstance, latest?: AutomationRunBrief): StatusTone {
  if (instance.status === "closed") return "muted";
  if (latest?.status === "waiting") return "caution";
  if (latest && isActiveRunStatus(latest.status)) return "active";
  return "nominal";
}

/** The Close verb with its confirmation, for an open agent. */
export function CloseAgentButton({
  instance,
  title,
}: {
  instance: AutomationInstance;
  title: string;
}) {
  const close = useCloseInstance();
  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button variant="outline" size="sm" disabled={close.isPending}>
          Close
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Close {title}?</AlertDialogTitle>
          <AlertDialogDescription>
            Matching events stop firing for this agent. Its activity stays available.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            onClick={async () => {
              try {
                await close.mutateAsync({ id: instance.id });
                toast.success(`Closed ${title}`);
              } catch (error) {
                toast.error(errorMessage(error));
              }
            }}
          >
            Close agent
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

const WITH_AUTOMATION = "grid-cols-[minmax(0,1.5fr)_150px_minmax(0,1.3fr)_130px_90px_72px]";
const WITHOUT_AUTOMATION = "grid-cols-[minmax(0,1.5fr)_minmax(0,1.3fr)_130px_90px_72px]";

export function AgentsTable({
  instances,
  automations,
  showAutomation,
  now,
  describe = describeAgent,
  handleLabel,
}: {
  instances: readonly AutomationInstance[];
  automations: readonly AutomationSummary[];
  showAutomation: boolean;
  now: number;
  describe?: DescribeAgent;
  handleLabel?: HandleLabel;
}) {
  const columns = showAutomation ? WITH_AUTOMATION : WITHOUT_AUTOMATION;
  return (
    <div className="overflow-x-auto rounded-lg border bg-card" role="table">
      <div className={showAutomation ? "min-w-[920px]" : "min-w-[760px]"}>
        <div
          role="row"
          className={`grid ${columns} gap-4 border-b px-4 py-2.5 text-xs font-semibold text-muted-foreground`}
        >
          <span role="columnheader">Agent</span>
          {showAutomation && <span role="columnheader">Automation</span>}
          <span role="columnheader">Where it lives</span>
          <span role="columnheader">Last run</span>
          <span role="columnheader">Started</span>
          <span role="columnheader" className="sr-only">
            Actions
          </span>
        </div>
        <div className="divide-y">
          {instances.map((instance) => (
            <AgentRow
              key={instance.id}
              instance={instance}
              automationName={
                automations.find((summary) => summary.automation?.id === instance.automationId)
                  ?.automation?.name
              }
              showAutomation={showAutomation}
              columns={columns}
              now={now}
              describe={describe}
              {...(handleLabel ? { handleLabel } : {})}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

function AgentRow({
  instance,
  automationName,
  showAutomation,
  columns,
  now,
  describe,
  handleLabel,
}: {
  instance: AutomationInstance;
  automationName?: string;
  showAutomation: boolean;
  columns: string;
  now: number;
  describe: DescribeAgent;
  handleLabel?: HandleLabel;
}) {
  const detail = useInstance(instance.id);
  const runs = useRunList(instance.automationId, { instanceId: instance.id, limit: 1 });
  const latest = runs.data?.runs[0];
  const tone = agentTone(instance, latest);
  const handles = detail.data?.handles.map((h) => h.handle) ?? [];
  const { title, subtitle } = describe(instance, handles);

  return (
    <div
      role="row"
      data-testid="agent-row"
      className={`grid ${columns} items-center gap-4 px-4 py-3.5 text-sm`}
    >
      <div role="cell" className="flex min-w-0 items-start gap-2.5">
        <StatusDot tone={tone} size={8} label={instance.status} className="mt-1.5" />
        <div className="min-w-0">
          <Link
            to="/automations/agents/$id"
            params={{ id: instance.id }}
            className="block truncate font-semibold underline-offset-4 hover:underline"
            title={instance.key}
          >
            {title}
          </Link>
          <span className="block truncate font-mono text-2xs text-muted-foreground">
            {subtitle}
          </span>
        </div>
      </div>
      {showAutomation && (
        <span role="cell" className="truncate">
          {automationName ?? "Unknown automation"}
        </span>
      )}
      <HandleCell
        handles={handles}
        pending={detail.isPending}
        {...(handleLabel ? { handleLabel } : {})}
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
      <span role="cell" className="flex justify-end">
        {instance.status === "open" && <CloseAgentButton instance={instance} title={title} />}
      </span>
    </div>
  );
}
