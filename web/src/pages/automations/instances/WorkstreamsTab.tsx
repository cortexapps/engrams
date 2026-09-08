import { useState } from "react";
import { PlusIcon } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useInstanceList, useRecentDrops } from "@/hooks/useInstances";
import type { InputFieldSpec } from "@/lib/automation-inputs";
import { OpenWorkstreamDialog } from "@/pages/automations/workstreams/OpenWorkstreamDialog";
import { WorkstreamDrops } from "@/pages/automations/workstreams/WorkstreamDrops";
import { WorkstreamsTable } from "@/pages/automations/workstreams/WorkstreamsTable";

export interface WorkstreamsTabProps {
  automationId: string;
  /** The current version's input schema for a new workstream. */
  inputsSchema: InputFieldSpec[];
  /** The automation's values become the defaults for a new workstream. */
  defaultInputs: unknown;
  /** Tests inject one stable clock for all relative times. */
  now?: () => number;
}

export function WorkstreamsTab({
  automationId,
  inputsSchema,
  defaultInputs,
  now = Date.now,
}: WorkstreamsTabProps) {
  const [status, setStatus] = useState<"open" | "closed">("open");
  const [dialogOpen, setDialogOpen] = useState(false);
  const list = useInstanceList(automationId, { includeClosed: true });
  const drops = useRecentDrops(automationId);
  const tick = now();
  const all = list.data?.instances ?? [];
  const open = all.filter((instance) => instance.status === "open");
  const closed = all.filter((instance) => instance.status === "closed");
  const rows = status === "open" ? open : closed;

  return (
    <section className="flex flex-col gap-3" aria-label="Workstreams">
      <div className="flex flex-wrap items-center gap-3">
        <Tabs value={status} onValueChange={(value) => setStatus(value as "open" | "closed")}>
          <TabsList aria-label="Workstream status">
            <TabsTrigger value="open">Open</TabsTrigger>
            <TabsTrigger value="closed">
              Closed
              <span className="font-mono text-2xs tabular-nums text-muted-foreground">
                {closed.length}
              </span>
            </TabsTrigger>
          </TabsList>
        </Tabs>
        <Button type="button" size="sm" className="ml-auto" onClick={() => setDialogOpen(true)}>
          <PlusIcon className="size-3.5" aria-hidden />
          Open a workstream
        </Button>
      </div>

      {list.error ? (
        <EmptyState tone="error">Couldn’t load workstreams. {String(list.error)}</EmptyState>
      ) : list.isPending ? (
        <div className="rounded-lg border bg-card px-4" data-testid="workstreams-loading">
          <SkeletonRows
            rows={3}
            columns={["minmax(0,1.5fr)", "minmax(0,1.3fr)", "130px", "90px"]}
          />
        </div>
      ) : rows.length === 0 ? (
        <EmptyState>
          {status === "open"
            ? "No open workstreams. Open one or wait for a matching event."
            : "No closed workstreams."}
        </EmptyState>
      ) : (
        <WorkstreamsTable instances={rows} automations={[]} showAutomation={false} now={tick} />
      )}

      <WorkstreamDrops drops={drops.data?.drops ?? []} now={tick} />
      <OpenWorkstreamDialog
        automations={[{ id: automationId, name: "This automation", inputsSchema, defaultInputs }]}
        instances={all}
        fixedAutomationId={automationId}
        open={dialogOpen}
        onOpenChange={setDialogOpen}
      />
    </section>
  );
}
