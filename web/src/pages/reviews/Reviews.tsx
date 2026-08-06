import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";

import { PageHeading } from "../../components/page-heading";
import { LivePulse } from "../../components/LivePulse";
import { useNow } from "../../hooks/useNow";
import { useReviews } from "../../hooks/useReviews";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { FilterBar, type FilterField } from "../sessions/filter-bar";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { X } from "lucide-react";
import { ReviewGlyph } from "./ReviewGlyph";
import { groupByPr, type PrGroup } from "./review-groups";
import {
  isActive,
  prTitleOf,
  reviewCreatedAt,
  severityCounts,
  severityTone,
  stageOf,
} from "./review-format";
import { cn } from "@/lib/utils";

// The reviews ledger: one row per reviewed PULL REQUEST, newest pass first.
//
// This replaces a table with one row per pass, which repeated the same PR once
// per retry and once per push and buried which pass was current. Rows are
// navigable — the dossier is a route, so a review is linkable from Slack and
// from the summary comment engrams posts on the PR itself — rather than
// expanding in place.
//
// The rail beside this page is the switcher; this page is for scanning and
// filtering many at once, which a rail can't do.

export function Reviews() {
  const { data, error, isPending } = useReviews();
  const now = useNow();
  const [query, setQuery] = useState("");
  const search = useDebouncedValue(query, 150).trim().toLowerCase();
  const [filters, setFilters] = useState<Record<string, string[]>>({});

  const groups = useMemo(() => groupByPr(data?.reviews ?? []), [data?.reviews]);

  const fields: FilterField[] = useMemo(() => {
    const repos = [...new Set(groups.map((g) => g.repo))].sort();
    const stages = [...new Set(groups.map((g) => g.latest.status))].sort();
    return [
      {
        key: "repo",
        label: "Repository",
        options: repos.map((repo) => ({ value: repo, label: repo })),
      },
      {
        key: "stage",
        label: "Stage",
        options: stages.map((status) => ({
          value: status,
          label: stageOf(status).label,
        })),
      },
      {
        key: "severity",
        label: "Severity",
        options: ["critical", "high", "medium", "low"].map((s) => ({
          value: s,
          label: s,
        })),
      },
    ];
  }, [groups]);

  const rows = useMemo(() => {
    const repo = filters["repo"] ?? [];
    const stage = filters["stage"] ?? [];
    const severity = filters["severity"] ?? [];
    return groups.filter((group) => {
      if (repo.length > 0 && !repo.includes(group.repo)) return false;
      if (stage.length > 0 && !stage.includes(group.latest.status)) return false;
      if (severity.length > 0) {
        const counts = group.latest.findingCounts;
        const hit = severity.some((s) => (counts?.[s as "critical"] ?? 0) > 0);
        if (!hit) return false;
      }
      if (search) {
        const title = prTitleOf(group.latest) ?? "";
        const haystack = `${group.repo} #${group.prNumber} ${title}`.toLowerCase();
        if (!haystack.includes(search)) return false;
      }
      return true;
    });
  }, [groups, filters, search]);

  const hasFilters = query !== "" || Object.values(filters).some((v) => v.length > 0);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-auto p-4 md:p-6">
      <PageHeading title="Reviews" />

      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search pull requests…"
          aria-label="Search pull requests"
          className="max-w-xs"
        />
        <FilterBar fields={fields} value={filters} onChange={setFilters} />
        {hasFilters && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setQuery("");
              setFilters({});
            }}
          >
            <X className="size-3.5" aria-hidden />
            Clear
          </Button>
        )}
      </div>

      {isPending ? (
        <ul
          className="overflow-hidden rounded-lg border"
          role="status"
          aria-label="Loading reviews"
        >
          {Array.from({ length: 5 }).map((_, i) => (
            <li key={i} className="flex items-center gap-3 border-b px-3 py-3 last:border-b-0">
              <Skeleton className="size-3 rounded-full" />
              <Skeleton className="h-4 flex-1" />
              <Skeleton className="h-4 w-24" />
            </li>
          ))}
        </ul>
      ) : error && groups.length === 0 ? (
        <div role="alert" className="rounded-lg border border-dashed py-12 text-center">
          <p className="text-sm text-destructive">Couldn’t load reviews. {errorMessage(error)}</p>
        </div>
      ) : rows.length === 0 ? (
        <div className="flex flex-col items-center gap-3 rounded-lg border border-dashed py-16 text-center">
          <span className="text-xl leading-none">
            <ReviewGlyph status="queued" beat={false} />
          </span>
          <p className="max-w-sm text-sm text-muted-foreground">
            {groups.length === 0
              ? "No pull requests reviewed yet. Enrol a repository in settings, or ask for a review on a PR with @engrams review."
              : "No pull requests match these filters."}
          </p>
        </div>
      ) : (
        <>
          <ul className="overflow-hidden rounded-lg border">
            {rows.map((group) => (
              <li key={group.key} className="border-b border-border last:border-b-0">
                <PrRow group={group} now={now} />
              </li>
            ))}
          </ul>
          <p className="font-mono text-xs tabular-nums text-muted-foreground">
            {rows.length} of {groups.length} pull requests
          </p>
        </>
      )}
    </div>
  );
}

/**
 * One reviewed PR. The number is the stable identity and the title is what a
 * developer remembers, so they share the leading line; a PR reviewed before the
 * title was captured shows the number alone rather than a placeholder.
 */
function PrRow({ group, now }: { group: PrGroup; now: number }) {
  const review = group.latest;
  const title = prTitleOf(review);
  const at = reviewCreatedAt(review);
  const stage = stageOf(review.status);
  const counts = severityCounts(review);

  return (
    <Link
      to="/reviews/$id"
      params={{ id: review.id }}
      className="flex items-center gap-3 px-3 py-2.5 outline-none transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
    >
      <span className="w-3 shrink-0 text-[0.8rem] leading-none">
        <ReviewGlyph status={review.status} />
      </span>

      <span className="flex min-w-0 flex-1 flex-col">
        <span className="flex min-w-0 items-baseline gap-2">
          <span className="shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
            #{group.prNumber}
          </span>
          <span className={cn("truncate text-sm", title ? "font-medium" : "font-mono")}>
            {title ?? `Pull request #${group.prNumber}`}
          </span>
        </span>
        <span className="flex min-w-0 items-baseline gap-2 text-xs text-muted-foreground">
          <span className="truncate font-mono">{group.repo}</span>
          {group.passes.length > 1 && (
            <span className="shrink-0 tabular-nums">{group.passes.length} passes</span>
          )}
        </span>
      </span>

      {/* Severity counts as words, not a row of pills. The tone rides a dot
          rather than the text: amber-on-paper measures 2.5:1 as 12px type, and
          `index.css` sets the rule these tokens were chosen against — a coloured
          dot beside ink, never coloured body copy. The word names the severity,
          so the dot is redundant and AA holds. */}
      <span className="hidden shrink-0 items-baseline gap-2.5 sm:flex">
        {counts.length === 0 ? (
          <span className="text-xs text-muted-foreground">no findings</span>
        ) : (
          counts.map(([severity, count]) => (
            <span
              key={severity}
              className="inline-flex items-center gap-1 font-mono text-xs tabular-nums"
              title={`${count} ${severity}`}
            >
              <span
                aria-hidden
                className="size-1.5 rounded-full"
                style={{ backgroundColor: severityTone(severity) }}
              />
              {count} {severity.slice(0, 4)}
            </span>
          ))
        )}
      </span>

      <span className="flex w-24 shrink-0 items-center justify-end gap-1.5">
        {isActive(review) && <LivePulse />}
        <span className="text-xs">{stage.label}</span>
      </span>

      <span
        title={at?.toLocaleString()}
        className="w-9 shrink-0 text-right font-mono text-xs tabular-nums text-muted-foreground"
      >
        {at ? relativeTime(at.toISOString(), now) : "—"}
      </span>
    </Link>
  );
}
