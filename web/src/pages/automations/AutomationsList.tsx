/** The automation list. Built-ins pin to the top; every automation keeps its
 * trigger, latest activity, seven-day history, and enabled state in one row. */
import { Link, useNavigate } from "@tanstack/react-router";
import {
  Clock3Icon,
  CopyPlusIcon,
  LockIcon,
  PlusIcon,
  RadioTowerIcon,
  WebhookIcon,
} from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { PageHeading } from "@/components/page-heading";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import type { AutomationSummary } from "@/gen/engram/app/v1/automation_pb";
import {
  useAutomations,
  useBuiltinAutomation,
  useDuplicateAutomation,
  useSetAutomationEnabled,
} from "@/hooks/useAutomations";
import { parseDefinition } from "@/lib/automation-blocks";
import { automationStatusLabel } from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { relativeTime } from "@/lib/relative-time";
import { cn } from "@/lib/utils";
import { formatDuration } from "./runs/run-format";
import { Sparkline } from "./Sparkline";

export const PR_REVIEW_BUILTIN_KEY = "pr_review";

const TABLE_COLUMNS = ["minmax(0,1.6fr)", "minmax(0,1.2fr)", "150px", "110px", "90px"];
const TABLE_GRID =
  "grid grid-cols-[minmax(0,1.6fr)_minmax(0,1.2fr)_150px_110px_90px] items-center gap-x-4 px-4";

/** Built-ins first (stable by name), then the rest by name. */
export function orderAutomations(items: AutomationSummary[]): AutomationSummary[] {
  return [...items].sort((a, b) => {
    const ak = a.automation?.kind === "builtin" ? 0 : 1;
    const bk = b.automation?.kind === "builtin" ? 0 : 1;
    if (ak !== bk) return ak - bk;
    return (a.automation?.name ?? "").localeCompare(b.automation?.name ?? "");
  });
}

function TriggerIcon({ summary }: { summary: string }) {
  if (summary.startsWith("Every") || summary.startsWith("Cron")) {
    return <Clock3Icon className="size-3.5 shrink-0" />;
  }
  if (summary.startsWith("Webhook")) return <WebhookIcon className="size-3.5 shrink-0" />;
  return <RadioTowerIcon className="size-3.5 shrink-0" />;
}

function AutomationTableHeader() {
  return (
    <div
      role="row"
      className={cn(TABLE_GRID, "h-9 border-b text-xs font-semibold text-muted-foreground")}
    >
      <span role="columnheader">Automation</span>
      <span role="columnheader">Trigger</span>
      <span role="columnheader">Last run</span>
      <span role="columnheader">7 days</span>
      <span role="columnheader">On</span>
    </div>
  );
}

function AutomationRow({ summary }: { summary: AutomationSummary }) {
  const automation = summary.automation;
  const setEnabled = useSetAutomationEnabled();
  if (!automation) return null;

  const builtin = automation.kind === "builtin";
  const paused = !automation.enabled;
  const failed = summary.lastRun?.status === "failed";
  const trigger = summary.triggerSummary || "Trigger not configured";
  const hasWorkstreams = Boolean(
    parseDefinition(automation.version?.definitionJson).settings?.instance,
  );
  const fadedCell = paused && "opacity-60";
  const lastRun = summary.lastRun;
  const duration =
    lastRun?.startedAt && lastRun.endedAt
      ? formatDuration(lastRun.startedAt, lastRun.endedAt)
      : null;
  const failedToday = summary.runs7d.at(-1)?.failed ?? 0;
  const tone = paused ? "muted" : failed ? "critical" : lastRun ? "nominal" : "muted";

  const onEnabledChange = async (enabled: boolean) => {
    try {
      await setEnabled.mutateAsync({ id: automation.id, enabled });
      toast.success(enabled ? "Turned on" : "Paused");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <div
      role="row"
      data-testid="automation-row"
      data-kind={automation.kind}
      className={cn(TABLE_GRID, "h-14")}
      style={
        failed
          ? {
              backgroundColor:
                "color-mix(in oklch, var(--color-instrument-critical) 5%, transparent)",
            }
          : undefined
      }
    >
      <Link
        to="/automations/$id"
        params={{ id: automation.id }}
        search={{ tab: "build" }}
        className={cn(
          "group grid min-w-0 grid-cols-[8px_minmax(0,1fr)] items-center gap-x-2 rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
          fadedCell,
        )}
      >
        <StatusDot tone={tone} size={8} />
        <span className="min-w-0">
          <span className="flex min-w-0 items-center gap-1.5">
            <span className="truncate text-sm font-semibold group-hover:underline">
              {automation.name}
            </span>
            {builtin && (
              <Badge variant="secondary" className="shrink-0 gap-1 rounded-sm">
                <LockIcon className="size-3" aria-hidden />
                built-in
              </Badge>
            )}
            {hasWorkstreams && (
              <Badge variant="secondary" className="shrink-0 rounded-sm">
                workstreams
              </Badge>
            )}
          </span>
          {automation.description && (
            <span className="block truncate text-xs text-muted-foreground">
              {automation.description}
            </span>
          )}
        </span>
      </Link>

      <span
        role="cell"
        className={cn(
          "flex min-w-0 items-center gap-1.5 font-mono text-xs text-muted-foreground",
          fadedCell,
        )}
      >
        <TriggerIcon summary={trigger} />
        <span className="truncate">{trigger}</span>
      </span>

      <span
        role="cell"
        className={cn("flex min-w-0 items-center gap-1 text-xs text-muted-foreground", fadedCell)}
      >
        {lastRun ? (
          <>
            <span>{automationStatusLabel(lastRun.status)}</span>
            <span className="truncate font-mono tabular-nums">
              {relativeTime(lastRun.startedAt)}
              {duration && ` · ${duration}`}
            </span>
            {failed && (
              <Badge
                variant="secondary"
                className="shrink-0 rounded-sm"
                style={{
                  backgroundColor:
                    "color-mix(in oklch, var(--color-instrument-critical) 14%, transparent)",
                }}
              >
                {failedToday} today
              </Badge>
            )}
          </>
        ) : (
          <span>never run</span>
        )}
      </span>

      <Link
        to="/automations/$id"
        params={{ id: automation.id }}
        search={{ tab: "activity" }}
        className={cn(
          "w-fit rounded-md p-1 outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring/50",
          fadedCell,
        )}
        aria-label={`Activity for ${automation.name}`}
      >
        <Sparkline
          days={summary.runs7d}
          label={`${automation.name}: activity over the last 7 days`}
        />
      </Link>

      <label className="inline-flex items-center gap-2 text-xs">
        <Switch
          aria-label={`${automation.enabled ? "Pause" : "Turn on"} ${automation.name}`}
          checked={automation.enabled}
          disabled={setEnabled.isPending}
          onCheckedChange={onEnabledChange}
        />
        {automation.enabled ? "On" : "Paused"}
      </label>
    </div>
  );
}

function DuplicatePrReviewButton() {
  const builtin = useBuiltinAutomation(PR_REVIEW_BUILTIN_KEY);
  const duplicate = useDuplicateAutomation();
  const navigate = useNavigate();
  const source = builtin.data?.automation;
  if (!source) return null;

  const onClick = async () => {
    try {
      const result = await duplicate.mutateAsync({ automationId: source.id });
      toast.success("Duplicated — edit the copy freely");
      const id = result.automation?.id;
      if (id) {
        await navigate({
          to: "/automations/$id",
          params: { id },
          search: { tab: "build" },
        });
      }
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <Button variant="outline" size="sm" onClick={onClick} disabled={duplicate.isPending}>
      <CopyPlusIcon className="size-3.5" />
      Duplicate PR review
    </Button>
  );
}

export function AutomationsList() {
  const automations = useAutomations();
  const rows = orderAutomations(automations.data?.automations ?? []);

  return (
    <div className="space-y-8">
      <PageHeading
        title="Automations"
        count={String(rows.length)}
        actions={
          <>
            <DuplicatePrReviewButton />
            <Button asChild size="sm">
              <Link to="/automations/new">
                <PlusIcon className="size-3.5" />
                New automation
              </Link>
            </Button>
          </>
        }
      />

      <section aria-labelledby="automation-list-heading">
        <h2 id="automation-list-heading" className="sr-only">
          Automations
        </h2>
        {automations.isPending && (
          <div role="table" className="overflow-x-auto rounded-lg border bg-card">
            <div className="min-w-[860px]">
              <AutomationTableHeader />
              <SkeletonRows
                rows={3}
                columns={TABLE_COLUMNS}
                className="px-4 [&>div]:h-14 [&>div]:py-0"
              />
            </div>
          </div>
        )}
        {automations.error && (
          <EmptyState tone="error">{errorMessage(automations.error)}</EmptyState>
        )}
        {!automations.isPending && !automations.error && rows.length === 0 && (
          <EmptyState
            action={
              <div className="flex flex-wrap justify-center gap-2">
                <DuplicatePrReviewButton />
                <Button asChild>
                  <Link to="/automations/new">New automation</Link>
                </Button>
              </div>
            }
          >
            No automations yet. Start from the built-in review pipeline or from scratch.
          </EmptyState>
        )}
        {!automations.isPending && !automations.error && rows.length > 0 && (
          <div role="table" className="overflow-x-auto rounded-lg border bg-card">
            <div className="min-w-[860px]">
              <AutomationTableHeader />
              <div role="rowgroup" className="divide-y">
                {rows.map((summary) => (
                  <AutomationRow key={summary.automation?.id} summary={summary} />
                ))}
              </div>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}
