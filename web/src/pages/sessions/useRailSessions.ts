import { useRouterState } from "@tanstack/react-router";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useTasksInfiniteAsSessionList } from "../../hooks/useTasks";
import type { SessionListItem, SessionState, ProfileSnapshotView } from "../../lib/types";
import { useRailStore } from "./rail-store";

// The ordered, capped list of the caller's OWN tasks that backs BOTH the
// sessions rail and the keyboard jump layer (⌥1–9 / ⌥[ ⌥]). Lifting it here
// is what keeps the rail's visible numbers and the jump targets in lockstep:
// the rail renders these rows, and the shortcuts navigate to rows[n-1] of the
// same array. Both consumers share React Query's cache, so there's no extra
// fetch.
//
// ADR 0051 Task 28: migrated from useSessions (REST /api/v1/sessions) to
// useTasksAsSessionList (connect-query ListTasks → orchestrator native
// TaskService). The REST surface is no longer reachable from the browser after
// the vite proxy flips all /api to the orchestrator. The explicit `mine` scope
// preserves the old own-only rail for members and admins alike.

/** One rail row. */
export interface RailRow {
  id: string;
  status: SessionState;
  /** ADR 0107: waiting on the user (plan review / question). */
  needsAttention?: boolean;
  image: string;
  at: string;
  profile?: ProfileSnapshotView | null;
  /** Effective display title (custom ?? harness suggestion ?? truncated
   * prompt), or null → the rail falls back to the short id. */
  title: string | null;
}
const fromListItem = (s: SessionListItem): RailRow => ({
  id: s.id,
  status: s.status,
  needsAttention: s.needsAttention ?? false,
  image: s.image,
  at: s.last_active_at,
  profile: s.profile,
  title: s.title,
});

export interface RailSessions {
  /** The rendered/jumpable rows, in display order. */
  rows: RailRow[];
  /** The currently-open session id, if the route is a session detail. */
  openId: string | undefined;
  /** True total of "my" sessions before pagination. */
  total: number;
  /** Whether another page-size step is available. */
  hasMore: boolean;
  isFetchingMore: boolean;
  fetchMore: ReturnType<typeof useTasksInfiniteAsSessionList>["fetchNextPage"];
  isPending: boolean;
  error: unknown;
}

export function useRailSessions(): RailSessions {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const search = useRailStore((s) => s.search);
  const debounced = useDebouncedValue(search);

  // `/sessions/<id>` → the open session; `/sessions/all` (fleet list) and
  // `/sessions/list` (my-tasks table) are section pages, not a detail — treating
  // either as a session id would aim the rail highlight and the ⌥[ / ⌥] anchor
  // at the literal id "all" / "list".
  const seg = pathname.startsWith("/sessions/") ? pathname.split("/")[2] : undefined;
  const openId = seg && seg !== "all" && seg !== "list" ? seg : undefined;

  // ADR 0051 Task 28: use TaskService-backed list instead of REST /sessions.
  const { data, totalCount, hasNextPage, isFetchingNextPage, fetchNextPage, isPending, error } =
    useTasksInfiniteAsSessionList({ scope: "mine", search: debounced }, 25);
  const all = data ?? [];

  // The rows ARE the window — nothing is pinned on top of it. An open session
  // that the window does not contain simply has no row: viewing a session is a
  // read, and it must not mutate the list. This used to prepend the open
  // session, which put another user's task (reachable for admins from the fleet
  // list) at the top of a list titled "My tasks" — a row above the server's
  // ordering, belonging to no page, unreachable again once you navigated away.
  // A task of your own climbs into the window on its own: any event bumps
  // `last_event_at`, the same clock the orchestrator sorts this list by.
  const rows: RailRow[] = all.map(fromListItem);

  const total = totalCount ?? 0;
  return {
    rows,
    openId,
    total,
    hasMore: hasNextPage,
    isFetchingMore: isFetchingNextPage,
    fetchMore: fetchNextPage,
    isPending,
    error,
  };
}
