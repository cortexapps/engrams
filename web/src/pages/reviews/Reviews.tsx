import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";

import { PageHeading } from "../../components/page-heading";
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
import { ReviewGlyph, ReviewStage } from "./ReviewGlyph";
import { Sep } from "./Sep";
import { groupByPr, type PrGroup } from "./review-groups";
import {
  diffShort,
  isActive,
  PR_STATES,
  prTitleOf,
  reviewCreatedAt,
  severityCounts,
  stageOf,
  SEVERITY_BAR_TONE,
  type Severity,
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

  // Every option is derived from the rows in hand, so a filter never offers a
  // value that would return nothing.
  const fields: FilterField[] = useMemo(() => {
    const distinct = <T,>(values: Array<T | undefined>) =>
      [...new Set(values.filter((v): v is T => v != null && v !== ""))].sort();
    const authors = distinct(groups.map((g) => g.latest.prAuthor));
    const states = distinct(groups.map((g) => g.latest.prState));
    return [
      {
        key: "repo",
        label: "Repository",
        options: distinct(groups.map((g) => g.repo)).map((repo) => ({ value: repo, label: repo })),
      },
      {
        key: "author",
        label: "Author",
        options: authors.map((author) => ({ value: author, label: author })),
      },
      {
        key: "pr_state",
        label: "Pull request",
        options: states.map((state) => ({
          value: state,
          label: PR_STATES[state]?.label ?? state,
        })),
      },
      {
        key: "stage",
        label: "Stage",
        options: distinct(groups.map((g) => g.latest.status)).map((status) => ({
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
    const author = filters["author"] ?? [];
    const prState = filters["pr_state"] ?? [];
    const stage = filters["stage"] ?? [];
    const severity = filters["severity"] ?? [];
    return groups.filter((group) => {
      const review = group.latest;
      if (repo.length > 0 && !repo.includes(group.repo)) return false;
      if (author.length > 0 && !(review.prAuthor && author.includes(review.prAuthor))) return false;
      if (prState.length > 0 && !(review.prState && prState.includes(review.prState))) return false;
      if (stage.length > 0 && !stage.includes(review.status)) return false;
      if (severity.length > 0) {
        const counts = review.findingCounts;
        const hit = severity.some((s) => (counts?.[s as "critical"] ?? 0) > 0);
        if (!hit) return false;
      }
      if (search) {
        // The author is searchable because the row shows it — a fact on screen
        // that the search box ignores reads as a broken search.
        const title = prTitleOf(review) ?? "";
        const haystack =
          `${group.repo} #${group.prNumber} ${title} ${review.prAuthor ?? ""}`.toLowerCase();
        if (!haystack.includes(search)) return false;
      }
      return true;
    });
  }, [groups, filters, search]);

  const hasFilters = query !== "" || Object.values(filters).some((v) => v.length > 0);

  // One count, in the masthead. It used to be stated twice — a chip up here and
  // a line under the list — which agree exactly whenever nothing is filtered.
  const noun = groups.length === 1 ? "pull request" : "pull requests";
  const count =
    groups.length === 0
      ? undefined
      : rows.length === groups.length
        ? `${groups.length} ${noun}`
        : `${rows.length} of ${groups.length} ${noun}`;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-6 overflow-auto p-4 md:p-6">
      <PageHeading title="Reviews" count={count} />

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
        // Two lines, because the row has two. A one-line silhouette under a
        // two-line row makes the list jump the moment the fetch lands.
        <ul
          className="overflow-hidden rounded-lg border"
          role="status"
          aria-label="Loading reviews"
        >
          {Array.from({ length: 5 }).map((_, i) => (
            <li key={i} className="flex items-center gap-3 border-b px-3 py-2.5 last:border-b-0">
              <span className="flex min-w-0 flex-1 flex-col gap-1.5">
                <Skeleton className="h-3.5 w-2/3" />
                <Skeleton className="h-3 w-40" />
              </span>
              <Skeleton className="h-1.5 w-16 rounded-full" />
              <Skeleton className="h-3 w-20" />
              <Skeleton className="h-3 w-9" />
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
        <ul className="overflow-hidden rounded-lg border">
          {rows.map((group) => (
            <li key={group.key} className="border-b border-border last:border-b-0">
              <PrRow group={group} now={now} />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * Where the bar's track is full. Past this the count beside it carries the
 * number; a pass with 60 findings and one with 200 are both "a lot", and
 * scaling to the largest row in view would make a row mean different things
 * under different filters.
 */
const BAR_FULL_AT = 12;

/**
 * The pass's findings: how many, and of what.
 *
 * It replaces four count chips whose block was as wide as its widest row — so
 * the numbers landed at a different x on every line, and four figures that beg
 * to be compared down a column could not be. The track is identical on every
 * row, so both channels are comparable down the page: the FILLED LENGTH is the
 * volume, and the segments inside it are the mix.
 *
 * Length has to carry volume. A bar that always filled its track drew one
 * medium finding as a full amber rail — louder on the page than a row carrying
 * twenty-three.
 *
 * Segments always run critical to low, so position says as much as hue does.
 */
function SeverityBar({ counts, total }: { counts: Array<[Severity, number]>; total: number }) {
  // A single finding still has to be visible, so the fill has a floor.
  const fill = Math.max(Math.min(total / BAR_FULL_AT, 1), 0.09);
  return (
    <span aria-hidden className="h-1.5 w-16 overflow-hidden rounded-full bg-muted">
      <span className="flex h-full gap-px" style={{ width: `${fill * 100}%` }}>
        {counts.map(([severity, count]) => (
          <span
            key={severity}
            // Proportional, with a floor: one critical among thirty lows is the
            // most important thing on the row and must not round away to nothing.
            style={{
              flexGrow: count,
              minWidth: "0.1875rem",
              background: SEVERITY_BAR_TONE[severity],
            }}
          />
        ))}
      </span>
    </span>
  );
}

/**
 * One reviewed PR. The number is the stable identity and the title is what a
 * developer remembers, so they share the leading line; a PR reviewed before the
 * title was captured shows the number alone rather than a placeholder.
 *
 * The pass's stage glyph is NOT in the left margin — it rides its own word in
 * the stage column. A coloured mark at one end of the row and the word that
 * explains it at the other satisfies the contrast rule in the markup and not in
 * the eye. The row opens with what the row is about: the pull request.
 */
function PrRow({ group, now }: { group: PrGroup; now: number }) {
  const review = group.latest;
  const title = prTitleOf(review);
  const at = reviewCreatedAt(review);
  const counts = severityCounts(review);
  const total = review.findingCounts?.total ?? 0;
  const prState = PR_STATES[review.prState ?? ""];
  const diff = diffShort(review);
  const StateIcon = prState?.icon;

  return (
    <Link
      to="/reviews/$id"
      params={{ id: review.id }}
      className="flex items-center gap-3 px-3 py-2.5 outline-none transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
    >
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="flex min-w-0 items-center gap-2">
          {StateIcon && (
            <span className="flex shrink-0 items-center text-muted-foreground">
              <StateIcon className="size-3.5" aria-hidden />
              <span className="sr-only">{prState.label} pull request</span>
            </span>
          )}
          <span className="shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
            #{group.prNumber}
          </span>
          <span className={cn("truncate text-sm", title ? "font-medium" : "font-mono")}>
            {title ?? `Pull request #${group.prNumber}`}
          </span>
        </span>
        <span className="flex min-w-0 items-baseline gap-2 text-xs text-muted-foreground">
          <span className="truncate font-mono">{group.repo}</span>
          {/* The separator hides WITH the thing it separates. Hidden on its own
              it leaves a row of orphan dots after the repo on a phone.
              A login is a name, so it reads as language — mono here would dress
              a person up as machine data. */}
          {review.prAuthor && (
            <span className="hidden min-w-0 items-baseline gap-2 sm:flex">
              <Sep />
              <span className="truncate">{review.prAuthor}</span>
            </span>
          )}
          {diff && (
            <span className="hidden shrink-0 items-baseline gap-2 sm:flex">
              <Sep />
              <span className="font-mono tabular-nums">{diff}</span>
            </span>
          )}
          {group.passes.length > 1 && (
            <>
              <Sep />
              <span className="shrink-0 tabular-nums">{group.passes.length} passes</span>
            </>
          )}
        </span>
      </span>

      {/* Findings: the mix, then the scale. A pass still running is silent here
          rather than reporting "no findings" — it has not looked yet, and a
          zero it has not earned is a false statement about the code. */}
      <span className="hidden w-24 shrink-0 items-center justify-end gap-2 sm:flex">
        {total > 0 ? (
          <>
            <SeverityBar counts={counts} total={total} />
            {/* Fixed width, or a two-digit count pushes the bar left and the
                track stops being a column to read down. */}
            <span className="w-6 text-right font-mono text-xs tabular-nums">
              {total}
              <span className="sr-only">
                {" "}
                findings: {counts.map(([s, n]) => `${n} ${s}`).join(", ")}
              </span>
            </span>
          </>
        ) : isActive(review) ? null : (
          <span className="text-xs text-muted-foreground">none</span>
        )}
      </span>

      {/* The mark and its word, together. The glyph beats on its own while the
          pass works, so it is the liveness signal too. */}
      {/* Left-aligned inside a fixed cell, so the glyphs form a column and every
          stage word starts at the same x. Right-aligned, they were ragged. */}
      <ReviewStage status={review.status} className="w-24 shrink-0 text-xs" />

      <span
        title={at?.toLocaleString()}
        className="w-9 shrink-0 text-right font-mono text-xs tabular-nums text-muted-foreground"
      >
        {at ? relativeTime(at.toISOString(), now) : "—"}
      </span>
    </Link>
  );
}
