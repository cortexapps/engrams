/** The automation list. Built-ins pin to the top; every automation keeps its
 * trigger, tags, latest activity, seven-day history, and enabled state in one
 * row. The tag filter lives in the URL. */
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Clock3Icon, LockIcon, PlusIcon, RadioTowerIcon, TagIcon, WebhookIcon } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { PageHeading } from "@/components/page-heading";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import { TagSwatch } from "@/components/tags/TagChip";
import { TagChipRow } from "@/components/tags/TagChipRow";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import type { AutomationSummary } from "@/gen/engram/app/v1/automation_pb";
import { useAutomations, useSetAutomationEnabled } from "@/hooks/useAutomations";
import { useTags } from "@/hooks/useTags";
import { parseDefinition, scopeLabel } from "@/lib/automation-blocks";
import { automationStatusLabel } from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { relativeTime } from "@/lib/relative-time";
import { matchesTagFilter, NO_TAGS, parseTagSearch } from "@/lib/tag-filter";
import { cn } from "@/lib/utils";
import { FilterBar, type FilterField } from "../sessions/filter-bar";
import { formatDuration } from "./runs/run-format";
import { Sparkline } from "./Sparkline";

export const PR_REVIEW_BUILTIN_KEY = "pr_review";

// One set of tracks for the header, the rows and the loading skeleton, applied
// as an inline template so the three cannot drift.
const TABLE_COLUMNS = [
  "minmax(0,1.6fr)",
  "minmax(0,1fr)",
  "minmax(0,0.8fr)",
  "minmax(200px,0.9fr)",
  "64px",
  "90px",
];
const TABLE_TEMPLATE = { gridTemplateColumns: TABLE_COLUMNS.join(" ") };
const TABLE_GRID = "grid items-center gap-x-4 px-4";

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
      style={TABLE_TEMPLATE}
    >
      <span role="columnheader">Automation</span>
      <span role="columnheader">Tags</span>
      <span role="columnheader">Trigger</span>
      <span role="columnheader">Last run</span>
      <span role="columnheader">7 days</span>
      <span role="columnheader">On</span>
    </div>
  );
}

function AutomationRow({
  summary,
  selectedTags,
}: {
  summary: AutomationSummary;
  /** The tag filter's selection; those tags show first. */
  selectedTags: readonly string[];
}) {
  const automation = summary.automation;
  const setEnabled = useSetAutomationEnabled();
  if (!automation) return null;

  const builtin = automation.kind === "builtin";
  const paused = !automation.enabled;
  const failed = summary.lastRun?.status === "failed";
  const trigger = summary.triggerSummary || "Trigger not configured";
  const scope = scopeLabel(parseDefinition(automation.version?.definitionJson));
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
              ...TABLE_TEMPLATE,
              backgroundColor:
                "color-mix(in oklch, var(--color-instrument-critical) 5%, transparent)",
            }
          : TABLE_TEMPLATE
      }
    >
      <div role="cell" className={cn("min-w-0", fadedCell)}>
        <Link
          to="/automations/$id"
          params={{ id: automation.id }}
          search={{ tab: "build" }}
          className="group grid min-w-0 grid-cols-[8px_minmax(0,1fr)] items-center gap-x-2 rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
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
              {/* In a narrow table the scope badge gives its width to the name. */}
              {scope && (
                <Badge variant="secondary" className="shrink-0 rounded-sm @max-[68rem]:hidden">
                  {scope}
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
      </div>

      <span role="cell" className={cn("min-w-0", fadedCell)}>
        <TagChipRow tags={automation.tags} selected={selectedTags} />
      </span>

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
            <span className="whitespace-nowrap font-mono tabular-nums">
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

export function AutomationsList() {
  const automations = useAutomations();
  const tags = useTags();
  const navigate = useNavigate();
  const selectedTags = parseTagSearch(useSearch({ strict: false }));
  const setSelectedTags = (next: string[]) =>
    void navigate({
      to: "/automations",
      search: next.length > 0 ? { tags: next } : {},
      replace: true,
    });

  const loaded = !automations.isPending && !automations.error;
  const rows = orderAutomations(automations.data?.automations ?? []);
  const visible = rows.filter((summary) =>
    matchesTagFilter(
      (summary.automation?.tags ?? []).map((tag) => tag.name),
      selectedTags,
    ),
  );
  const knownTags = (tags.data?.tags ?? []).flatMap((summary) =>
    summary.tag ? [summary.tag] : [],
  );
  const tagNames = knownTags.map((tag) => tag.name);
  // A name in the URL that matches no tag (renamed, deleted) stays an option,
  // so the admin can see it is selected and remove it.
  const staleNames = selectedTags.filter((name) => name !== NO_TAGS && !tagNames.includes(name));
  const tagField: FilterField = {
    key: "tag",
    label: "Tag",
    icon: <TagIcon className="size-3.5" />,
    options: [
      ...knownTags.map((tag) => ({
        value: tag.name,
        label: tag.name,
        icon: <TagSwatch color={tag.color} />,
      })),
      ...staleNames.map((name) => ({ value: name, label: name })),
      { value: NO_TAGS, label: "No tags" },
    ],
  };
  const showTagFilter = tagNames.length > 0 || selectedTags.length > 0;

  return (
    <div className="space-y-8">
      <PageHeading
        title="Automations"
        count={
          selectedTags.length > 0 ? `${visible.length} of ${rows.length}` : String(rows.length)
        }
        actions={
          <Button asChild size="sm">
            <Link to="/automations/new">
              <PlusIcon className="size-3.5" />
              New automation
            </Link>
          </Button>
        }
      />

      <section aria-labelledby="automation-list-heading">
        <h2 id="automation-list-heading" className="sr-only">
          Automations
        </h2>
        {automations.isPending && (
          <div role="table" className="@container overflow-x-auto rounded-lg border bg-card">
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
        {loaded && rows.length > 0 && (
          <div className="mb-3 flex flex-wrap items-center gap-2">
            {showTagFilter && (
              <FilterBar
                fields={[tagField]}
                value={{ tag: selectedTags }}
                onChange={(next) => setSelectedTags(next["tag"] ?? [])}
              />
            )}
            <Button asChild variant="ghost" size="sm">
              <Link to="/settings/tags">Manage tags</Link>
            </Button>
          </div>
        )}
        {loaded && rows.length > 0 && visible.length === 0 && (
          <EmptyState
            action={
              <Button variant="outline" onClick={() => setSelectedTags([])}>
                Clear filter
              </Button>
            }
          >
            No automations have these tags.
          </EmptyState>
        )}
        {loaded && rows.length === 0 && (
          <EmptyState
            action={
              <Button asChild>
                <Link to="/automations/new">New automation</Link>
              </Button>
            }
          >
            No automations yet. Describe one and the drafting agent builds it, or build it on the
            canvas.
          </EmptyState>
        )}
        {loaded && visible.length > 0 && (
          <div role="table" className="@container overflow-x-auto rounded-lg border bg-card">
            <div className="min-w-[860px]">
              <AutomationTableHeader />
              <div role="rowgroup" className="divide-y">
                {visible.map((summary) => (
                  <AutomationRow
                    key={summary.automation?.id}
                    summary={summary}
                    selectedTags={selectedTags}
                  />
                ))}
              </div>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}
