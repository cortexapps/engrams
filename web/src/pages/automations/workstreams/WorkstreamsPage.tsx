import { useState } from "react";
import { PlusIcon } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { PageHeading } from "@/components/page-heading";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useAutomations } from "@/hooks/useAutomations";
import { useAllInstances, useAllRecentDrops } from "@/hooks/useInstances";
import { useNow } from "@/hooks/useNow";
import { parseDefinition } from "@/lib/automation-blocks";
import { parseInputsSchema } from "@/lib/automation-inputs";

import { OpenWorkstreamDialog, type WorkstreamAutomationOption } from "./OpenWorkstreamDialog";
import { WorkstreamDrops } from "./WorkstreamDrops";
import { WorkstreamsTable } from "./WorkstreamsTable";

const ALL_AUTOMATIONS = "__all__";

function parseJsonObject(value: string): unknown {
  try {
    const parsed: unknown = JSON.parse(value);
    return typeof parsed === "object" && parsed !== null ? parsed : {};
  } catch {
    return {};
  }
}

export function WorkstreamsPage() {
  const automations = useAutomations();
  const summaries = automations.data?.automations ?? [];
  const ids = summaries.map((summary) => summary.automation?.id).filter((id): id is string => !!id);
  const all = useAllInstances(ids, { includeClosed: true });
  const drops = useAllRecentDrops(ids);
  const [status, setStatus] = useState<"open" | "closed">("open");
  const [picked, setPicked] = useState(ALL_AUTOMATIONS);
  const [dialogOpen, setDialogOpen] = useState(false);
  const now = useNow(10_000);
  const relevant = all.instances.filter(
    (instance) => picked === ALL_AUTOMATIONS || instance.automationId === picked,
  );
  const open = relevant.filter((instance) => instance.status === "open");
  const closed = relevant.filter((instance) => instance.status === "closed");
  const rows = status === "open" ? open : closed;
  const options: WorkstreamAutomationOption[] = summaries.flatMap((summary) => {
    const automation = summary.automation;
    if (!automation) return [];
    const definition = parseDefinition(automation.version?.definitionJson);
    if (!definition.settings.instance) return [];
    return [
      {
        id: automation.id,
        name: automation.name,
        inputsSchema: parseInputsSchema(definition.inputsSchema),
        defaultInputs: parseJsonObject(automation.inputsJson),
      },
    ];
  });
  const error = automations.error ?? all.error ?? drops.error;
  const pending = automations.isPending || (ids.length > 0 && all.isPending);

  return (
    <div className="space-y-6">
      <PageHeading
        title="Workstreams"
        count={pending ? undefined : `${open.length} open`}
        actions={
          <Button type="button" size="sm" onClick={() => setDialogOpen(true)}>
            <PlusIcon className="size-3.5" aria-hidden />
            Open a workstream
          </Button>
        }
      />

      <section className="space-y-3" aria-label="Workstreams">
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
          <Select value={picked} onValueChange={setPicked}>
            <SelectTrigger
              size="sm"
              className="h-7 w-[200px] bg-background"
              aria-label="Filter by automation"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_AUTOMATIONS}>All automations</SelectItem>
              {summaries.map((summary) =>
                summary.automation ? (
                  <SelectItem key={summary.automation.id} value={summary.automation.id}>
                    {summary.automation.name}
                  </SelectItem>
                ) : null,
              )}
            </SelectContent>
          </Select>
        </div>

        {error ? (
          <EmptyState tone="error">Couldn’t load workstreams. {String(error)}</EmptyState>
        ) : pending ? (
          <div className="rounded-lg border bg-card px-4">
            <SkeletonRows
              rows={4}
              columns={["minmax(0,1.5fr)", "150px", "minmax(0,1.3fr)", "130px", "90px"]}
            />
          </div>
        ) : rows.length === 0 ? (
          <EmptyState>
            {status === "open"
              ? "No open workstreams. Open one or wait for a matching event."
              : "No closed workstreams."}
          </EmptyState>
        ) : (
          <WorkstreamsTable instances={rows} automations={summaries} showAutomation now={now} />
        )}
      </section>

      <WorkstreamDrops drops={drops.drops} now={now} />
      <OpenWorkstreamDialog
        automations={options}
        instances={all.instances}
        open={dialogOpen}
        onOpenChange={setDialogOpen}
      />
    </div>
  );
}
