import { useMemo, useState } from "react";
import { useSearch } from "@tanstack/react-router";

import { useEditorAutomation } from "@/hooks/useAutomationEditor";
import { useRunList } from "@/hooks/useAutomationRuns";
import { useInstanceList, useRecentDrops } from "@/hooks/useInstances";
import { blockTypesOf, parseDefinition } from "@/lib/automation-blocks";

import { ActivityList } from "./ActivityList";

// One automation's activity (the editor's Activity tab), or one agent's
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
  // A delivery that could not fire has no run row; the ledger shows it in
  // its place in time. An agent's own timeline has none (a drop belongs to
  // no agent).
  const drops = useRecentDrops(automationId, instanceId === undefined);

  // Name the agent on a bound run — fetched only once one is visible.
  const anyBound = (runs.data?.runs ?? []).some((run) => run.instanceId !== "");
  const instances = useInstanceList(automationId, {
    includeClosed: true,
    enabled: anyBound && instanceId === undefined,
  });
  const keyById = new Map(
    (instances.data?.instances ?? []).map((instance) => [instance.id, instance.key]),
  );

  // The current version's block kinds name what each step did. A run on an
  // older version whose block is gone falls back to the generic readout.
  const automation = useEditorAutomation(automationId);
  const definitionJson = automation.data?.automation?.version?.definitionJson;
  const blockTypes = useMemo(() => blockTypesOf(parseDefinition(definitionJson)), [definitionJson]);

  return (
    <ActivityList
      runs={runs.data?.runs ?? []}
      windows={(runs.data?.filtered ?? []).map((w) => ({ ...w, automationId }))}
      drops={drops.data?.drops ?? []}
      isPending={runs.isPending}
      error={runs.error}
      now={tick}
      includeFiltered={includeFiltered}
      onIncludeFilteredChange={setIncludeFiltered}
      agentLabelOf={instanceId === undefined ? (id) => keyById.get(id) : undefined}
      blockTypeOf={(_automationId, blockId) => blockTypes.get(blockId)}
      superseded
      openRunId={search.run}
    />
  );
}
