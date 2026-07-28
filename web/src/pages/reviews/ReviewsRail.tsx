import { useMemo, useState, type CSSProperties } from "react";
import { Link, useRouterState } from "@tanstack/react-router";
import { ListChecks, TriangleAlert } from "lucide-react";

import { useNow } from "../../hooks/useNow";
import { useReviews } from "../../hooks/useReviews";
import { relativeTime } from "../sessions/session-format";
import { LivePulse } from "../../components/LivePulse";
import { ReviewGlyph } from "./ReviewGlyph";
import { groupByPr } from "./review-groups";
import { isActive, prTitleOf, reviewCreatedAt, stageOf } from "./review-format";
import {
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInput,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSkeleton,
} from "@/components/ui/sidebar";

// The persistent reviews rail: a switcher between reviewed PULL REQUESTS, not
// between passes. A retry and every push mint another `review` row, so a
// pass-per-row rail would show the same PR three times and bury which pass is
// current; each row here is one PR carrying its newest pass's stage, and it
// navigates to that pass.
//
// The rail stays mounted across the ledger and the dossier (the dossier is a
// child route), so opening a PR moves the highlight instead of swapping the
// layout — the same shape as the sessions rail.

export function ReviewsRail() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const { data, isPending, error } = useReviews();
  const now = useNow();
  const [search, setSearch] = useState("");

  const groups = useMemo(() => groupByPr(data?.reviews ?? []), [data?.reviews]);

  // Match on what the row actually shows — the repo, the number, and the title
  // when we have one — so searching for "quinn" finds the PR a developer
  // remembers by name rather than by coordinate.
  const rows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return groups;
    return groups.filter((g) => {
      const title = prTitleOf(g.latest) ?? "";
      return (
        g.repo.toLowerCase().includes(needle) ||
        String(g.prNumber).includes(needle) ||
        title.toLowerCase().includes(needle)
      );
    });
  }, [groups, search]);

  // The dossier route is /reviews/<id>; the ledger index is /reviews itself.
  const openId = pathname.startsWith("/reviews/") ? pathname.split("/")[2] : undefined;
  const onLedger = pathname === "/reviews" || pathname === "/reviews/";

  return (
    <>
      <SidebarHeader className="gap-2 px-2 pt-3">
        {/* Keyed to the sidebar palette rather than stock bg-background, which
            paints near-white under the rail's inherited sage ink in light mode
            (the same fix the sessions rail carries). */}
        <SidebarInput
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search pull requests…"
          aria-label="Search pull requests"
          className="border-sidebar-border bg-sidebar-accent/40 text-sidebar-foreground placeholder:text-sidebar-foreground/80 dark:bg-sidebar-accent/40"
        />
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup className="py-1">
          <SidebarGroupLabel>Reviewed PRs</SidebarGroupLabel>
          <SidebarGroupContent>
            {/* The review glyph reads content-surface instrument tokens, which
                lose contrast on the deep-green rail. Re-tone them to the
                sidebar's own palette inside the list only; the glyph stays
                correct on every other surface. */}
            <SidebarMenu
              style={
                {
                  "--muted-foreground":
                    "color-mix(in oklch, var(--sidebar-foreground) 72%, transparent)",
                  "--instrument-nominal": "var(--sidebar-ring)",
                  "--instrument-caution":
                    "color-mix(in oklch, var(--sidebar-foreground) 88%, transparent)",
                  "--instrument-critical":
                    "color-mix(in oklch, var(--destructive) 55%, var(--sidebar-foreground))",
                } as CSSProperties
              }
            >
              {isPending ? (
                Array.from({ length: 5 }).map((_, i) => (
                  <SidebarMenuItem key={i}>
                    <SidebarMenuSkeleton />
                  </SidebarMenuItem>
                ))
              ) : error ? (
                // Destructive red is near-invisible on the rail's ground; carry
                // the error on legible sage ink plus an alert glyph instead.
                <p className="flex items-center gap-1.5 px-2 py-1.5 text-xs text-sidebar-foreground">
                  <TriangleAlert className="size-3.5 shrink-0" />
                  Couldn’t load reviews.
                </p>
              ) : rows.length === 0 ? (
                <p className="px-2 py-2 text-xs text-sidebar-foreground/85">
                  {search ? "No matching pull requests." : "No reviews yet."}
                </p>
              ) : (
                rows.map((group) => {
                  const review = group.latest;
                  const title = prTitleOf(review);
                  const total = review.findingCounts?.total ?? 0;
                  const at = reviewCreatedAt(review);
                  return (
                    <SidebarMenuItem key={group.key}>
                      <SidebarMenuButton
                        asChild
                        isActive={group.passes.some((p) => p.id === openId)}
                        className="h-auto items-start gap-2.5 py-1.5 data-[active=true]:font-medium"
                      >
                        <Link
                          to="/reviews/$id"
                          params={{ id: review.id }}
                          title={`${group.repo}#${group.prNumber}`}
                        >
                          {/* The glyph is aria-hidden by contract and the rail has
                              no room for the stage word, so the word goes to
                              assistive tech only — otherwise a rail row announces
                              a PR with no indication of what happened to it. */}
                          <span className="mt-0.5 shrink-0 text-[0.7rem] leading-none">
                            <ReviewGlyph status={review.status} />
                            <span className="sr-only">{stageOf(review.status).label}</span>
                          </span>
                          <span className="flex min-w-0 flex-1 flex-col">
                            {/* Number and title share a line: the number is the
                                stable identity, the title is what the developer
                                actually remembers. Untitled PRs (every review
                                recorded before capture landed) keep the number
                                alone rather than showing a blank. */}
                            <span className="flex min-w-0 items-baseline gap-1.5">
                              <span className="shrink-0 font-mono text-[0.7rem] leading-tight tabular-nums text-sidebar-foreground/85">
                                #{group.prNumber}
                              </span>
                              {title && (
                                <span className="truncate text-[0.8rem] leading-tight">
                                  {title}
                                </span>
                              )}
                            </span>
                            <span className="flex min-w-0 items-baseline gap-1.5 text-[0.7rem] leading-tight text-sidebar-foreground/85">
                              <span className="truncate font-mono">{group.repo}</span>
                              {total > 0 && (
                                <span className="shrink-0 tabular-nums">
                                  · {total} {total === 1 ? "finding" : "findings"}
                                </span>
                              )}
                              {group.passes.length > 1 && (
                                <span className="shrink-0 tabular-nums">
                                  · {group.passes.length} passes
                                </span>
                              )}
                            </span>
                          </span>
                          <span className="flex min-w-[1.4rem] shrink-0 items-start justify-end gap-1 self-stretch leading-none">
                            {isActive(review) && (
                              <>
                                <LivePulse className="mt-1" />
                                <span className="sr-only">Running now</span>
                              </>
                            )}
                            {at && (
                              <span
                                title={at.toLocaleString()}
                                className="mt-0.5 font-mono text-[0.65rem] tabular-nums text-sidebar-foreground/85"
                              >
                                {relativeTime(at.toISOString(), now)}
                              </span>
                            )}
                          </span>
                        </Link>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  );
                })
              )}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      {/* The rail is the switcher; the ledger is the place to scan and filter
          many at once, so it stays an explicit destination. */}
      <SidebarFooter className="gap-1">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild isActive={onLedger}>
              <Link to="/reviews">
                <ListChecks />
                <span>All reviews</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
    </>
  );
}
