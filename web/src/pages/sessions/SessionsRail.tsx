import { useCallback, type CSSProperties } from "react";
import { ListChecks, Plus, TriangleAlert } from "lucide-react";
import { Link, useRouterState } from "@tanstack/react-router";
import { useIsAdmin } from "../../auth/AuthProvider";
import { StatusGlyph } from "../../components/Glyph";
import { useKeyboardUi } from "../../keyboard/store";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Kbd } from "@/components/ui/kbd";
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
import { useRailSessions } from "./useRailSessions";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import { useRailStore } from "./rail-store";
import { useLoadMoreSentinel } from "../../hooks/useLoadMoreSentinel";

// The persistent sessions rail: a live switcher between the caller's tasks that
// stays mounted across the list views AND the transcript (the rail is the
// second sidebar of the whole /sessions section). The server supplies recency
// order; the open session is highlighted (the selected-session crumb).
//
// The ordering comes from useRailSessions, the same hook the ⌥-jump keymap
// reads — so the 1–9 numbers this rail reveals while ⌥ is held point at exactly
// the rows ⌥1…⌥9 navigate to.

export function SessionsRail() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const requestComposerFocus = useKeyboardUi((s) => s.requestComposerFocus);
  const jumpHeld = useKeyboardUi((s) => s.jumpHeld);
  const search = useRailStore((s) => s.search);
  const setSearch = useRailStore((s) => s.setSearch);
  const showMore = useRailStore((s) => s.showMore);

  const onStart = pathname === "/sessions" || pathname === "/sessions/";
  const onAllList = pathname.startsWith("/sessions/all");

  const { rows, openId, hasMore, isPending, error } = useRailSessions();
  // Row count is intentionally a dependency: it gives the sentinel hook a new
  // callback to observe after each larger ListTasks response arrives.
  const handleLoadMore = useCallback(showMore, [showMore, rows.length]);
  const loadMoreRef = useLoadMoreSentinel({ hasMore, onLoadMore: handleLoadMore });

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
        <SidebarGroup className="py-1">
          <SidebarGroupLabel>My tasks</SidebarGroupLabel>
          <SidebarGroupContent>
            {/* StatusGlyph reads content-surface vars (--ring, --muted-foreground)
                which go near-invisible on the dark section rail. Remap them to
                the sidebar's own palette within the list: active → lime ring,
                idle/archived → a visible faded sage. The glyph stays correct
                everywhere else; only this context re-tones it. */}
            <SidebarMenu
              style={
                {
                  "--ring": "var(--sidebar-ring)",
                  "--muted-foreground":
                    "color-mix(in oklch, var(--sidebar-foreground) 72%, transparent)",
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
                // Destructive red is ~1.5:1 on the deep-green rail (invisible in
                // light mode); carry the error on legible sage ink + an alert
                // glyph instead — glyph + word, the product's status grammar.
                <p className="flex items-center gap-1.5 px-2 py-1.5 text-xs text-sidebar-foreground">
                  <TriangleAlert className="size-3.5 shrink-0" />
                  Couldn’t load tasks.
                </p>
              ) : rows.length === 0 ? (
                <p className="px-2 py-2 text-xs text-sidebar-foreground/70">
                  {search ? "No matching tasks." : "No tasks yet."}
                </p>
              ) : (
                <>
                  {rows.map((r, i) => {
                    const showNum = jumpHeld && i < 9;
                    return (
                      <SidebarMenuItem key={r.id}>
                        <SidebarMenuButton
                          asChild
                          isActive={r.id === openId}
                          className="h-auto items-start gap-2.5 py-1.5 data-[active=true]:font-medium"
                        >
                          <Link to="/sessions/$id" params={{ id: r.id }} title={r.id}>
                            <span className="mt-0.5 shrink-0 text-[0.7rem] leading-none">
                              <StatusGlyph status={r.status} />
                            </span>
                            <span className="flex min-w-0 flex-1 flex-col">
                              <span className="truncate font-mono text-[0.8rem] leading-tight">
                                {shortId(r.id)}
                              </span>
                              <ProfileChip
                                profile={r.profile}
                                fallbackImage={r.image}
                                disclosure="tooltip"
                                className="text-[0.7rem] leading-tight text-sidebar-foreground/70"
                              />
                            </span>
                            {/* Trailing slot crossfades the relative time with the
                                ⌥-jump number while the modifier is held. Stretches
                                the full row height (self-stretch) so the time keeps
                                the top line while the badge centers vertically; the
                                fixed footprint stops the row reflowing on reveal. */}
                            <span className="relative flex min-w-[1.4rem] shrink-0 items-start justify-end self-stretch leading-none">
                              <span
                                className={cn(
                                  "mt-0.5 font-mono text-[0.65rem] tabular-nums text-sidebar-foreground/70 transition-opacity duration-150 motion-reduce:transition-none",
                                  showNum && "opacity-0",
                                )}
                              >
                                {relativeTime(r.at)}
                              </span>
                              {i < 9 && (
                                <span
                                  aria-hidden
                                  className={cn(
                                    "absolute inset-0 flex items-center justify-end transition-opacity duration-150 motion-reduce:transition-none",
                                    showNum ? "opacity-100" : "opacity-0",
                                  )}
                                >
                                  <Badge className="min-w-5 justify-center rounded-md px-1.5 py-1 font-display font-semibold leading-none tabular-nums bg-sidebar-primary text-sidebar-primary-foreground">
                                    {i + 1}
                                  </Badge>
                                </span>
                              )}
                            </span>
                          </Link>
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    );
                  })}
                  {hasMore && (
                    <SidebarMenuItem
                      ref={loadMoreRef}
                      aria-hidden
                      className="py-1 text-center text-xs text-sidebar-foreground/70"
                    >
                      …
                    </SidebarMenuItem>
                  )}
                </>
              )}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
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
