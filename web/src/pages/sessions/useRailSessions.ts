import { useRouterState } from "@tanstack/react-router";
import { useSession } from "../../hooks/useSessions";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import type { Session, SessionListItem, SessionState, ProfileSnapshotView } from "../../lib/types";
import { compareSessions } from "./session-format";

// The ordered, capped, open-session-pinned list that backs BOTH the sessions
// rail and the keyboard jump layer (⌥1–9 / ⌥[ ⌥]). Lifting it here is what
// keeps the rail's visible numbers and the jump targets in lockstep: the rail
// renders these rows, and the shortcuts navigate to rows[n-1] of the same
// array. Both consumers share React Query's cache, so there's no extra fetch.
//
// ADR 0051 Task 28: migrated from useSessions (REST /api/v1/sessions) to
// useTasksAsSessionList (connect-query ListTasks → orchestrator native
// TaskService). The REST surface is no longer reachable from the browser after
// the vite proxy flips all /api to the orchestrator. One deliberate scope
// change: ListTasks has no scope param, so an ADMIN's rail now previews ALL
// tasks (incl. unattributed rows) like the list page, where the old
// useSessions("mine") was own-only even for admins. Members are unaffected
// (server-side CASL scoping). Otherwise behaviour is unchanged:
// member sees own tasks, admin sees all (CASL gate on the server).

export const RAIL_CAP = 10;

/** One rail row. Normalises the list shape (`last_active_at`) and the single
 * session shape (`created_at`, used to pin an open session outside the recent
 * window) to a single `at` timestamp. */
export interface RailRow {
  id: string;
  status: SessionState;
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
  image: s.image,
  at: s.last_active_at,
  profile: s.profile,
  title: s.title,
});
const fromSession = (s: Session): RailRow => ({
  id: s.id,
  status: s.status,
  image: s.image,
  at: s.created_at,
  profile: null,
  title: null,
});

// Most-recently-active first (status-agnostic); the stable sort keeps rows that
// share a timestamp from jittering under the 1s refetch. The full sessions list
// (SessionsList) shares this exact comparator, so the rail reads as a capped
// preview of the same order.
function sortForRail(rows: SessionListItem[]): SessionListItem[] {
  return [...rows].sort(compareSessions);
}

export interface RailSessions {
  /** The rendered/jumpable rows, in display order. */
  rows: RailRow[];
  /** The currently-open session id, if the route is a session detail. */
  openId: string | undefined;
  /** True total of "my" sessions (the rail caps the visible rows at RAIL_CAP). */
  total: number;
  isPending: boolean;
  error: unknown;
}

export function useRailSessions(): RailSessions {
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  // `/sessions/<id>` → the open session; `/sessions/all` (fleet list) and
  // `/sessions/list` (my-tasks table) are section pages, not a detail — treating
  // either as a session id would poll GetSession({ sessionId: "list" }) on a loop.
  const seg = pathname.startsWith("/sessions/") ? pathname.split("/")[2] : undefined;
  const openId = seg && seg !== "all" && seg !== "list" ? seg : undefined;

  // ADR 0051 Task 28: use TaskService-backed list instead of REST /sessions.
  const { data, isPending, error } = useTasksAsSessionList();
  const all = data ?? [];
  const recent = sortForRail(all).slice(0, RAIL_CAP);

  // The open session always needs a row, even if it's older than the recent
  // window or (for an admin) isn't one of mine. Shares the query cache with
  // SessionDetail's own useSession, so it's not an extra fetch.
  const openSession = useSession(openId);
  const rows: RailRow[] = recent.map(fromListItem);
  if (openId && !rows.some((r) => r.id === openId)) {
    const inAll = all.find((s) => s.id === openId);
    if (inAll) rows.unshift(fromListItem(inAll));
    else if (openSession.data) rows.unshift(fromSession(openSession.data));
  }

  return { rows, openId, total: all.length, isPending, error };
}
