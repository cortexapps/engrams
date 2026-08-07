import type { CSSProperties } from "react";
import { ChevronRight, ListChecks, Plus, TriangleAlert } from "lucide-react";
import { Link, useRouterState } from "@tanstack/react-router";
import { useIsAdmin } from "../../auth/AuthProvider";
import { StatusGlyph } from "../../components/Glyph";
import { useKeyboardUi } from "../../keyboard/store";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Kbd } from "@/components/ui/kbd";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
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
import { relativeTime, shortId } from "./session-format";
import { useRailSessions, type RailRow } from "./useRailSessions";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import { useRailStore } from "./rail-store";
import { useLoadMoreSentinel } from "../../hooks/useLoadMoreSentinel";
import { useNow } from "../../hooks/useNow";

// The persistent sessions rail: a live switcher between the caller's tasks that
// stays mounted across the list views AND the transcript (the rail is the
// second sidebar of the whole /sessions section). The server supplies recency
// order; the open session is highlighted (the selected-session crumb).
//
// The ordering comes from useRailSessions, the same hook the ⌥-jump keymap
// reads — so the 1–9 numbers this rail reveals while ⌥ is held point at exactly
// the rows ⌥1…⌥9 navigate to.

const RAIL_GLYPH_VARS = {
  "--ring": "var(--sidebar-ring)",
  "--muted-foreground": "color-mix(in oklch, var(--sidebar-foreground) 72%, transparent)",
} as CSSProperties;

export function SessionsRail() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const requestComposerFocus = useKeyboardUi((s) => s.requestComposerFocus);
  const jumpHeld = useKeyboardUi((s) => s.jumpHeld);
  const search = useRailStore((s) => s.search);
  const setSearch = useRailStore((s) => s.setSearch);
  const openBands = useRailStore((s) => s.openBands);
  const setBandOpen = useRailStore((s) => s.setBandOpen);
  const now = useNow();

  const onStart = pathname === "/sessions" || pathname === "/sessions/";
  const onAllList = pathname.startsWith("/sessions/all");

  const { rows, groups, banded, openId, hasMore, isFetchingMore, fetchMore, isPending, error } =
    useRailSessions();
  const loadMoreRef = useLoadMoreSentinel({
    hasMore,
    isFetching: isFetchingMore,
    onLoadMore: fetchMore,
  });

  // Where each band starts in the VISIBLE row list, so a row's ⌥-jump number is
  // its position in the rail as a whole. A collapsed band contributes nothing —
  // the same rule `visibleRows` follows, which is what ⌥1–9 navigates.
  let seen = 0;
  const offsets = groups.map((group) => {
    const start = seen;
    if (!banded || openBands[group.band]) seen += group.items.length;
    return start;
  });

  // The server bands before it pages, so the next page lands at the bottom —
  // in the last band, or in one that sorts after it. Unbanded there is nothing
  // to collapse, so the sentinel is always live.
  const lastBand = groups[groups.length - 1];
  const lastBandOpen = !banded || (lastBand ? openBands[lastBand.band] : false);

  return (
    <>
      {/* px-2 matches the standard p-2 inset of the content group + footer, so
          the New task row, the recent rows, and the footer links are all the
          same width; pt-3 only adds a little breathing room at the rail top. */}
      <SidebarHeader className="gap-2 px-2 pt-3">
        {/* "New task" is now a destination, not a dialog: it leads to the start
            screen (the canonical create surface). Lighter than the old lime CTA
            — the composer there is the real "go" — but it still reads as the
            rail's primary action via the leading +, and the active wash when
            you're on it. Bumping the focus nonce drops the cursor in the
            composer even when the start screen is already mounted. */}
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild isActive={onStart} tooltip="New task">
              <Link to="/sessions" onClick={() => requestComposerFocus()}>
                <Plus />
                <span>New task</span>
                <Kbd className="ml-auto group-data-[collapsible=icon]:hidden">c</Kbd>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
        {/* The rail is a permanently dark surface (`.sidebar-section` re-tones
            --sidebar in BOTH themes), but stock SidebarInput paints
            bg-background — near-white in light mode — under the rail's
            inherited sage text. Key the field to the sidebar palette instead
            so text/placeholder stay legible in either theme. */}
        <SidebarInput
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search tasks…"
          aria-label="Search tasks"
          className="border-sidebar-border bg-sidebar-accent/40 text-sidebar-foreground placeholder:text-sidebar-foreground/60 dark:bg-sidebar-accent/40 group-data-[collapsible=icon]:hidden"
        />
      </SidebarHeader>

      {/* SidebarContent is the scroll container (min-h-0 flex-1 overflow-auto
          by default), sitting between the pinned header and footer. */}
      <SidebarContent>
        {isPending ? (
          <SidebarGroup className="py-1">
            <SidebarGroupContent>
              <SidebarMenu>
                {Array.from({ length: 5 }).map((_, i) => (
                  <SidebarMenuItem key={i}>
                    <SidebarMenuSkeleton />
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ) : error ? (
          // Destructive red is ~1.5:1 on the deep-green rail (invisible in
          // light mode); carry the error on legible sage ink + an alert
          // glyph instead — glyph + word, the product's status grammar.
          <p className="flex items-center gap-1.5 px-4 py-1.5 text-xs text-sidebar-foreground">
            <TriangleAlert className="size-3.5 shrink-0" />
            Couldn’t load tasks.
          </p>
        ) : rows.length === 0 ? (
          <p className="px-4 py-2 text-xs text-sidebar-foreground/70">
            {search ? "No matching tasks." : "No tasks yet."}
          </p>
        ) : (
          <>
            {/* The bands disappear when the control plane is unreachable, and a
                list that quietly changes shape reads as a bug. Say why once, at
                the top, in the same voice as the load error below it. */}
            {!banded && (
              <p className="flex items-center gap-1.5 px-4 pt-1 text-xs text-sidebar-foreground/70">
                <TriangleAlert className="size-3.5 shrink-0" />
                No live status right now.
              </p>
            )}
            {groups.map((group, gi) => (
              <Collapsible
                key={group.band}
                className="group/band"
                // Unbanded (no live session state): one group, always open, with
                // no trigger to close it. A disclosure control over a list that
                // has no other section to disclose is a control that does nothing.
                open={!banded || openBands[group.band]}
                onOpenChange={(open) => banded && setBandOpen(group.band, open)}
              >
                <SidebarGroup
                  // Space and a hairline separate the bands — never a box. The
                  // first band needs neither: the search field above it already
                  // ends the header.
                  className={cn("py-1", gi > 0 && "mt-2 border-t border-sidebar-border pt-3")}
                >
                  {/* No count. It could only ever report the loaded page, so on
                    the one band where a number would matter — the history at
                    the bottom, which pages — it would be a lower bound
                    presented as a total. */}
                  {banded && (
                    <SidebarGroupLabel asChild>
                      <CollapsibleTrigger className="w-full gap-1 hover:bg-sidebar-accent/40">
                        <ChevronRight
                          className="size-3 shrink-0 transition-transform duration-150 group-data-[state=open]/band:rotate-90 motion-reduce:transition-none"
                          aria-hidden
                        />
                        {group.label}
                      </CollapsibleTrigger>
                    </SidebarGroupLabel>
                  )}
                  <CollapsibleContent>
                    <SidebarGroupContent>
                      {/* StatusGlyph reads content-surface vars (--ring,
                        --muted-foreground) which go near-invisible on the dark
                        section rail. Remap them to the sidebar's own palette
                        within the list: active → lime ring, idle/archived → a
                        visible faded sage. The glyph stays correct everywhere
                        else; only this context re-tones it. */}
                      <SidebarMenu style={RAIL_GLYPH_VARS}>
                        {group.items.map((r, i) => (
                          <RailTaskRow
                            key={r.id}
                            row={r}
                            // Counts across every band, because the jump layer
                            // navigates to the nth row of the same flattened list.
                            index={offsets[gi]! + i}
                            open={r.id === openId}
                            jumpHeld={jumpHeld}
                            now={now}
                          />
                        ))}
                      </SidebarMenu>
                    </SidebarGroupContent>
                  </CollapsibleContent>
                </SidebarGroup>
              </Collapsible>
            ))}
          </>
        )}
        {/* Only while the band the next page would land in is open. Collapsed,
            the loaded rows stop growing the rail, so the sentinel would sit in
            view and pull page after page of history nobody asked to see — and
            the poll refetches every loaded page. */}
        {hasMore && lastBandOpen && (
          <div
            ref={loadMoreRef}
            aria-hidden
            className="py-1 text-center text-xs text-sidebar-foreground/70"
          >
            …
          </div>
        )}
      </SidebarContent>

      {/* The rail itself is the full my-tasks surface; admins retain the explicit
          fleet-wide destination. */}
      <SidebarFooter className="gap-1">
        <SidebarMenu>
          {isAdmin && (
            <SidebarMenuItem>
              <SidebarMenuButton asChild isActive={onAllList}>
                <Link to="/sessions/all">
                  <ListChecks />
                  <span>All tasks</span>
                </Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          )}
        </SidebarMenu>
      </SidebarFooter>
    </>
  );
}

/** One task in the rail: state glyph, name, what it runs on, and its age. */
function RailTaskRow({
  row,
  index,
  open,
  jumpHeld,
  now,
}: {
  row: RailRow;
  /** Position in the whole rail — the ⌥-jump target for `index + 1`. */
  index: number;
  open: boolean;
  jumpHeld: boolean;
  now: number;
}) {
  const showNum = jumpHeld && index < 9;
  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        asChild
        isActive={open}
        className="h-auto items-start gap-2.5 py-1.5 data-[active=true]:font-medium"
      >
        <Link to="/sessions/$id" params={{ id: row.id }} title={row.id}>
          <span className="mt-0.5 shrink-0 text-[0.7rem] leading-none">
            <StatusGlyph status={row.status} attention={row.needsAttention} />
          </span>
          <span className="flex min-w-0 flex-1 flex-col">
            <span
              className={cn(
                "truncate text-[0.8rem] leading-tight",
                row.title ? "font-medium" : "font-mono",
              )}
            >
              {row.title ?? shortId(row.id)}
            </span>
            <ProfileChip
              profile={row.profile}
              fallbackImage={row.image}
              disclosure="tooltip"
              className="text-[0.7rem] leading-tight text-sidebar-foreground/70"
            />
          </span>
          {/* Trailing slot crossfades the relative time with the ⌥-jump number
              while the modifier is held. Stretches the full row height
              (self-stretch) so the time keeps the top line while the badge
              centers vertically; the fixed footprint stops the row reflowing
              on reveal. */}
          <span className="relative flex min-w-[1.4rem] shrink-0 items-start justify-end self-stretch leading-none">
            <span
              className={cn(
                "mt-0.5 font-mono text-[0.65rem] tabular-nums text-sidebar-foreground/70 transition-opacity duration-150 motion-reduce:transition-none",
                showNum && "opacity-0",
              )}
            >
              {relativeTime(row.at, now)}
            </span>
            {index < 9 && (
              <span
                aria-hidden
                className={cn(
                  "absolute inset-0 flex items-center justify-end transition-opacity duration-150 motion-reduce:transition-none",
                  showNum ? "opacity-100" : "opacity-0",
                )}
              >
                <Badge className="min-w-5 justify-center rounded-md px-1.5 py-1 font-semibold leading-none tabular-nums bg-sidebar-primary text-sidebar-primary-foreground">
                  {index + 1}
                </Badge>
              </span>
            )}
          </span>
        </Link>
      </SidebarMenuButton>
    </SidebarMenuItem>
  );
}
