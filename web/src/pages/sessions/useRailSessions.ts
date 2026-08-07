import { useRouterState } from "@tanstack/react-router";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useTasksInfiniteAsSessionList } from "../../hooks/useTasks";
import {
  UNKNOWN_STATE,
  type ListRowState,
  type SessionListItem,
  type SessionState,
  type ProfileSnapshotView,
} from "../../lib/types";
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
  status: ListRowState;
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

/**
 * The band a task sits in, from the only question a reader asks a task list:
 * does this need me, is it working, is it asleep, is it over.
 *
 * It is NOT the lifecycle. Eleven machine states is the right vocabulary for a
 * row's own glyph and the wrong one for a header, and half of them (evacuating,
 * evicting, unreachable) are the platform moving a sandbox around, which is
 * never the reason a person opens their task list.
 */
export type RailBand = "attention" | "working" | "idle" | "finished";

/** Exhaustive by construction: a new `SessionState` is a compile error here,
 *  not a task that silently lands in the wrong band. */
const BAND_BY_STATE: Record<SessionState, RailBand> = {
  pending: "working",
  queued: "working",
  created: "working",
  active: "working",
  // The platform relocating or losing a sandbox. It resolves itself, and the
  // task is still the reader's live work while it does.
  unreachable: "working",
  evacuating: "working",
  evicting: "working",
  // Parked is a paused VM and idle is a snapshot. The difference is how fast a
  // prompt wakes it, which is the platform's problem, not the reader's.
  parked: "idle",
  idle: "idle",
  host_lost: "finished",
  completed: "finished",
  failed: "finished",
  dead: "finished",
};

const BAND_ORDER: RailBand[] = ["attention", "working", "idle", "finished"];

const BAND_LABEL: Record<RailBand, string> = {
  attention: "Needs you",
  working: "Working",
  idle: "Idle",
  finished: "Finished",
};

/** ADR 0107 attention outranks the lifecycle: a task waiting on an answer is
 *  waiting on YOU whether its sandbox is running or already snapshotted. */
function bandOf(row: RailRow): RailBand {
  if (row.needsAttention) return "attention";
  // `unknown` never reaches here — the caller stops banding entirely when the
  // control plane is unreachable — but the type says it can, so it gets the
  // band that keeps a row visible rather than one that buries it.
  return row.status === UNKNOWN_STATE ? "working" : BAND_BY_STATE[row.status];
}

export interface RailGroup {
  band: RailBand;
  label: string;
  items: RailRow[];
}

/** The one group the rail draws when there is no live session state: every
 *  loaded task, newest first, under no heading at all. It is not a band — it
 *  is the absence of banding, so it never collapses and never persists. */
const UNBANDED: RailBand = "working";

export interface RailSessions {
  /** Every loaded row, in display order — which is the banded order. This is
   *  the complete set, including the rows inside a collapsed band, because the
   *  command menu searches what you HAVE, not what you can currently see. */
  rows: RailRow[];
  /** The rows a reader can actually see: `rows` minus every collapsed band.
   *  The ⌥1–9 layer reads this, so a number always points at a row on screen. */
  visibleRows: RailRow[];
  /** The same rows, banded. Empty bands are dropped. When `banded` is false
   *  this is a single unlabelled group holding every row. */
  groups: RailGroup[];
  /** False while the control plane is unreachable: no row carries live state,
   *  so the rail draws a plain recency-ordered list with no headings and
   *  nothing to collapse. */
  banded: boolean;
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
  const openBands = useRailStore((s) => s.openBands);
  const debounced = useDebouncedValue(search);

  // `/sessions/<id>` → the open session; `/sessions/all` (fleet list) and
  // `/sessions/list` (my-tasks table) are section pages, not a detail — treating
  // either as a session id would aim the rail highlight and the ⌥[ / ⌥] anchor
  // at the literal id "all" / "list".
  const seg = pathname.startsWith("/sessions/") ? pathname.split("/")[2] : undefined;
  const openId = seg && seg !== "all" && seg !== "list" ? seg : undefined;

  // ADR 0051 Task 28: use TaskService-backed list instead of REST /sessions.
  const {
    data,
    totalCount,
    sessionStateAvailable,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    isPending,
    error,
  } = useTasksInfiniteAsSessionList({ scope: "mine", search: debounced }, 25);
  const all = data ?? [];

  // The rows ARE the window — nothing is pinned on top of it. An open session
  // that the window does not contain simply has no row: viewing a session is a
  // read, and it must not mutate the list. This used to prepend the open
  // session, which put another user's task (reachable for admins from the fleet
  // list) at the top of a list titled "My tasks" — a row above the server's
  // ordering, belonging to no page, unreachable again once you navigated away.
  // A task of your own climbs into the window on its own: any event bumps
  // `last_event_at`, the same clock the orchestrator sorts this list by.
  // Banded for display, recency-ordered inside each band. Both orders come from
  // the server, which sorts by band before it pages (`taskBandRank` in
  // orchestrator/src/rpc/tasks.ts) — so page one holds every live task rather
  // than whichever ones happened to be recent enough to make the cut.
  const windowed: RailRow[] = all.map(fromListItem);
  // No live session state means no bands. Every row would be `unknown`, so
  // "Working" and "Finished" would be a story we made up about an outage —
  // and with Finished collapsed by default the rail would look EMPTY. The
  // server drops to plain recency for the same reason, so this one group is
  // already in the order it should be read.
  const groups: RailGroup[] = sessionStateAvailable
    ? BAND_ORDER.map((band) => ({
        band,
        label: BAND_LABEL[band],
        items: windowed.filter((row) => bandOf(row) === band),
      })).filter((group) => group.items.length > 0)
    : windowed.length > 0
      ? [{ band: UNBANDED, label: "", items: windowed }]
      : [];
  const rows: RailRow[] = groups.flatMap((group) => group.items);
  // The jump layer navigates to visibleRows[n-1] and the rail draws n on the
  // nth row it RENDERS, so a collapsed band drops out of both together. An
  // unbanded list has nothing to collapse, so every row stays jumpable.
  const visibleRows: RailRow[] = sessionStateAvailable
    ? groups.filter((group) => openBands[group.band]).flatMap((group) => group.items)
    : rows;

  const total = totalCount ?? 0;
  return {
    rows,
    visibleRows,
    groups,
    banded: sessionStateAvailable,
    openId,
    total,
    hasMore: hasNextPage,
    isFetchingMore: isFetchingNextPage,
    fetchMore: fetchNextPage,
    isPending,
    error,
  };
}
