import { useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { toast } from "sonner";

import { useAutomation } from "@/hooks/useAutomations";
import { useRetryRun, useRun, useStopRun } from "@/hooks/useAutomationRuns";
import { errorMessage } from "@/lib/errors";
import { PageHeading } from "@/components/page-heading";
import { Button } from "@/components/ui/button";
import { RunStatusDot } from "./RunsTab";
import { RunTimeline } from "./RunTimeline";
import { StepRunDrawer } from "./StepRunDrawer";
import {
  buildTimeline,
  findTimelineStep,
  formatDuration,
  isActiveRunStatus,
  parseJsonObject,
  runStatusLabel,
  triggerSourceLabel,
} from "./run-format";

/** Block id → type from the pinned definition, for timeline labels. The
 * definition is the current version; a run pinned to an older version may
 * name blocks that no longer exist — those just render without a type. */
export function blockTypesFromDefinition(
  definitionJson: string | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  const definition = definitionJson ? parseJsonObject(definitionJson) : null;
  const walk = (blocks: unknown): void => {
    if (!Array.isArray(blocks)) return;
    for (const block of blocks) {
      if (typeof block !== "object" || block === null) continue;
      const b = block as Record<string, unknown>;
      if (typeof b["id"] === "string" && typeof b["type"] === "string") out[b["id"]] = b["type"];
      walk(b["then"]);
      walk(b["else"]);
      walk(b["body"]);
    }
  };
  walk(definition?.["blocks"]);
  return out;
}

export interface RunPageProps {
  /** Route params; tests pass them explicitly. */
  automationId?: string;
  runId?: string;
}

export function RunPage(props: RunPageProps = {}) {
  const params = useParams({ strict: false }) as { id?: string; runId?: string };
  const automationId = props.automationId ?? params.id ?? "";
  const runId = props.runId ?? params.runId ?? "";
  const navigate = useNavigate();

  const run = useRun(runId);
  const automation = useAutomation(automationId);
  const stop = useStopRun();
  const retry = useRetryRun();
  // Selection is keyed on the step's stable frame path, never the step
  // object: the run polls while active, and the drawer must show each poll's
  // status/outputs/attempts, not the snapshot captured at click time.
  const [selectedPath, setSelectedPath] = useState<string | null>(null);

  const blockTypes = useMemo(
    () => blockTypesFromDefinition(automation.data?.automation?.version?.definitionJson),
    [automation.data],
  );

  const steps = run.data?.run?.steps ?? [];
  const selected = useMemo(
    () => (selectedPath === null ? null : findTimelineStep(buildTimeline(steps), selectedPath)),
    [steps, selectedPath],
  );

  const brief = run.data?.run?.brief;
  const active = brief ? isActiveRunStatus(brief.status) : false;

  const onStop = async () => {
    try {
      await stop.mutateAsync({ runId, reason: "stopped from the run page" });
      toast.success("Stop requested");
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

  const onRetry = async () => {
    try {
      const res = await retry.mutateAsync({ runId });
      toast.success("Retry started");
      await navigate({
        to: "/settings/automations/$id/runs/$runId",
        params: { id: automationId, runId: res.runId },
      });
    } catch (err) {
      toast.error(errorMessage(err));
    }
  };

  return (
    <div className="flex flex-col gap-6">
      <PageHeading
        title={
          <span className="flex items-center gap-3">
            {brief && <RunStatusDot status={brief.status} className="size-2.5" />}
            <span>Run {runId.split(":").at(-1) ?? runId}</span>
          </span>
        }
        actions={
          <div className="flex items-center gap-2">
            {active && (
              <Button variant="outline" onClick={onStop} disabled={stop.isPending}>
                Stop
              </Button>
            )}
            {brief && !active && (
              <Button variant="outline" onClick={onRetry} disabled={retry.isPending}>
                {brief.status === "completed" ? "Re-run" : "Retry"}
              </Button>
            )}
          </div>
        }
      />

      <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-4">
        <div>
          <dt className="text-xs text-muted-foreground">Automation</dt>
          <dd>
            <Link
              to="/settings/automations/$id"
              params={{ id: automationId }}
              className="underline underline-offset-2"
            >
              {automation.data?.automation?.name ?? automationId}
            </Link>
          </dd>
        </div>
        <div>
          <dt className="text-xs text-muted-foreground">Trigger</dt>
          <dd>{brief ? triggerSourceLabel(brief.triggerSource, brief.eventKey) : "—"}</dd>
        </div>
        <div>
          <dt className="text-xs text-muted-foreground">Status</dt>
          <dd className="capitalize">
            {brief ? runStatusLabel(brief.status) : "—"}
            {brief?.dryRun && (
              <span className="ml-2 rounded bg-secondary px-1.5 text-xs">dry run</span>
            )}
          </dd>
        </div>
        <div>
          <dt className="text-xs text-muted-foreground">Duration</dt>
          <dd className="tabular-nums">
            {brief ? formatDuration(brief.startedAt, brief.endedAt) : "—"}
            {brief?.startedAt && (
              <span className="ml-2 text-xs text-muted-foreground">
                started {new Date(brief.startedAt).toLocaleString()}
              </span>
            )}
          </dd>
        </div>
      </dl>

      {brief?.error && (
        <p
          role="alert"
          className="rounded-md border border-instrument-critical/40 bg-instrument-critical/5 p-3 text-sm"
        >
          {brief.error}
        </p>
      )}

      <RunTimeline
        steps={steps}
        blockTypes={blockTypes}
        selectedPath={selectedPath ?? undefined}
        onSelect={(step) => setSelectedPath(step.path)}
      />

      <StepRunDrawer
        step={selected}
        blockType={selected ? blockTypes[selected.blockId] : undefined}
        onClose={() => setSelectedPath(null)}
      />
    </div>
  );
}
