import { useState } from "react";
import { useSearch } from "@tanstack/react-router";

import { useRunList } from "@/hooks/useAutomationRuns";
import { useInstanceList } from "@/hooks/useInstances";

import { ActivityList } from "./ActivityList";

// One automation's activity (the editor's Activity tab), or one workstream's
// timeline when `instanceId` narrows it. A `?run=` in the URL opens that
// entry's trace — the destination the old run page's links now land on.
export function ActivityTab({
  automationId,
  instanceId,
  now = Date.now,
}: {
  automationId: string;
  instanceId?: string;
  /** Injected by tests; defaults to the wall clock. */
  now?: () => number;
}) {
  const [includeFiltered, setIncludeFiltered] = useState(false);
  const search = useSearch({ strict: false }) as { run?: string };
  const runs = useRunList(automationId, {
    includeFiltered,
    ...(instanceId !== undefined ? { instanceId } : {}),
  });
  const tick = now();

  // Name the workstream on a bound run — fetched only once one is visible.
  const anyBound = (runs.data?.runs ?? []).some((run) => run.instanceId !== "");
  const instances = useInstanceList(automationId, {
    includeClosed: true,
    enabled: anyBound && instanceId === undefined,
  });
  const keyById = new Map(
    (instances.data?.instances ?? []).map((instance) => [instance.id, instance.key]),
  );

  return (
    <ActivityList
      runs={runs.data?.runs ?? []}
      windows={(runs.data?.filtered ?? []).map((w) => ({ ...w, automationId }))}
      isPending={runs.isPending}
      error={runs.error}
      now={tick}
      includeFiltered={includeFiltered}
      onIncludeFilteredChange={setIncludeFiltered}
      workstreamLabelOf={instanceId === undefined ? (id) => keyById.get(id) : undefined}
      superseded
      openRunId={search.run}
    />
  );
}
