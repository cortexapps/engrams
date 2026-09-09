import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";

import { PageHeading } from "../../components/page-heading";
import { useNow } from "../../hooks/useNow";
import { useReviewsInfinite } from "../../hooks/useReviews";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useLoadMoreSentinel } from "../../hooks/useLoadMoreSentinel";
import { errorMessage } from "../../lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { FilterBar, type FilterField } from "../sessions/filter-bar";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { X } from "lucide-react";
import { ReviewStage } from "./ReviewGlyph";
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

/** Pull requests per page. The page carries every pass of each. */
const PAGE_SIZE = 25;
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
//
// The list is paged by pull request and read a page at a time as the reader
// scrolls, the way the tasks ledger is. The search and every filter run on the
// server — over every reviewed pull request, not the pages in hand — so a
// filter never hides a match that sits on a page not yet read. The author is
// searchable because the row shows it: a fact on screen that the search box
// ignores reads as a broken search.

export function Reviews() {
  const now = useNow();
  const [query, setQuery] = useState("");
  const search = useDebouncedValue(query, 150).trim();
  const [filters, setFilters] = useState<Record<string, string[]>>({});

  const {
    reviews,
    totalCount,
    facets,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    isPending,
    error,
  } = useReviewsInfinite(
    {
      search,
      repos: filters["repo"],
      authors: filters["author"],
      prStates: filters["pr_state"],
      statuses: filters["stage"],
      severities: filters["severity"],
    },
    PAGE_SIZE,
  );
  const loadMoreRef = useLoadMoreSentinel({
    hasMore: hasNextPage,
    isFetching: isFetchingNextPage,
    onLoadMore: fetchNextPage,
  });

  const rows = useMemo(() => groupByPr(reviews), [reviews]);

  // The options are the server's facets over every reviewed pull request, so a
  // filter never offers a value that would return nothing — and never omits one
  // whose pull requests sit on a page the reader has not reached.
  const fields: FilterField[] = useMemo(
    () => [
      {
        key: "repo",
        label: "Repository",
        options: facets.repos.map((repo) => ({ value: repo, label: repo })),
      },
      {
        key: "author",
        label: "Author",
        options: facets.authors.map((author) => ({ value: author, label: author })),
      },
      {
        key: "pr_state",
        label: "Pull request",
        options: facets.prStates.map((state) => ({
          value: state,
          label: PR_STATES[state]?.label ?? state,
        })),
      },
      {
        key: "stage",
        label: "Stage",
        options: facets.statuses.map((status) => ({
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
    ],
    [facets],
  );

  const hasFilters = query !== "" || Object.values(filters).some((v) => v.length > 0);

  // One count, in the masthead: the pull requests matching the filters, which
  // the server knows whether or not the reader has scrolled to them all.
  const total = totalCount ?? 0;
  const noun = total === 1 ? "pull request" : "pull requests";
  const count = total === 0 ? undefined : `${total} ${noun}`;

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
        <SkeletonRows
          rows={5}
          columns={["minmax(0,1fr)", "64px", "80px", "36px"]}
          className="overflow-hidden rounded-lg border px-3"
        />
      ) : error && rows.length === 0 ? (
        <EmptyState tone="error">Couldn’t load reviews. {errorMessage(error)}</EmptyState>
      ) : rows.length === 0 ? (
        <EmptyState>
          {hasFilters
            ? "No pull requests match these filters."
            : "No pull requests reviewed yet. Enrol a repository in settings, or ask for a review on a PR with @engrams review."}
        </EmptyState>
      ) : (
        <>
          <ul className="overflow-hidden rounded-lg border">
            {rows.map((group) => (
              <li key={group.key} className="border-b border-border last:border-b-0">
                <PrRow group={group} now={now} />
              </li>
            ))}
          </ul>
          {hasNextPage && (
            <div ref={loadMoreRef} className="py-2">
              <SkeletonRows rows={1} />
            </div>
          )}
          {rows.length < total && (
            <p className="font-mono text-xs tabular-nums text-muted-foreground">
              {rows.length} of {total} {noun}
            </p>
          )}
        </>
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
    <span aria-hidden className="h-1.5 w-16 overflow-hidden rounded-sm bg-muted">
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
          <span className="text-xs text-muted-foreground">None</span>
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
        {at ? relativeAge(at.toISOString(), now) : "—"}
      </span>
    </Link>
  );
}
