# 0087 — Task-list filters: server-side scope, search, and pagination for ListTasks

Status: Accepted

Commit chain: cf65dd78 (ADR) → b9b32b32 (ListTasks filters + pagination) →
df74b6ff (web surfaces + composite FilterBar) → 93b7cba0 (rev 3: search into
the Drizzle where-clause) → fd1a7e35 (rev 4: infinite scroll, clamp 200→1000)
→ 54c2d936 (rev 5: pinned list chrome + virtualized rows).

As-built divergences from the original proposal: search moved from in-memory
into SQL once it was clear only the synthetic unattributed rows genuinely need
a post-join match (rev 3); the owner filter became `repeated` so the composite
filter bar needs no single-select special case (rev 2); every click-to-page
affordance (rail show-more, Prev/Next pagers) was replaced by sentinel-driven
infinite scroll over a growing page_size (rev 4); the list pages pin their
heading/toolbar and virtualize rows (rev 5), which retires the "pager UI"
described below wherever the two conflict; rev 6 replaced the flat 1s poll +
growing-page_size single query with connect-query `useInfiniteQuery`
(page-appends in the shared cache, dedupe-by-id across page seams) and
adaptive polling — 2s only while a LIVE session on a visible task is in a
transitional state (boot/evict bursts; a GC'd-session "pending" fallback does
not count), 30s ambient, refetch-on-focus, mutation-driven invalidation, and a
local 30s clock for relative-time labels. Page-appends are safe once the
constant poll is gone: full-chain refetches are rare and rebuild a consistent
snapshot.

## Context

`TaskService.ListTasks` (ADR 0051 §3) takes an empty request: the server returns
every row the caller's CASL ability can read. That made the first cut simple, but
it has aged badly on three fronts:

1. **An admin's "My tasks" is everyone's tasks.** The rail, the start screen's
   recent list, and `/sessions/list` all call `ListTasks {}`; for an admin the
   ability is `manage all`, so their personal surfaces are polluted with every
   user's tasks plus synthetic unattributed rows.
2. **No search.** Finding a task means scrolling; the only narrowing is the
   client-side Live/Archived tabs.
3. **No pagination.** Every surface fetches the full table (and the full
   `task_session` table, and the full control-plane session list) on a 1s poll.

The web wants: a sessions-rail that is simply *all of my tasks* (searchable,
paginated), and an admin **All tasks** page with reui-style filters — owner,
session state, free-text search — over a paginated list.

## Decision

### Wire contract (task.proto)

`ListTasksRequest` gains filter fields; `ListTasksResponse` gains `total_count`.
The security stance is preserved and restated: **filters narrow inside the
caller's ability; they never widen it.**

```proto
message ListTasksRequest {
  // "" = legacy ability default (member → own, admin → all + unattributed);
  // "mine" = strictly the caller's own tasks (admins included);
  // "all"  = fleet-wide + unattributed, admin-only (PermissionDenied otherwise).
  string scope = 1;
  // Case-insensitive substring over task title, task id, and session ids.
  string search = 2;
  // Owner filter: better-auth user ids and/or the sentinel "system" for
  // unattributed rows. OR within the field (rev 2: repeated, so the filter
  // bar's multi-select needs no special-cased single-owner path). Empty =
  // no owner filter.
  repeated string created_by_user_ids = 3;
  // Live session-state filter (e.g. "active", "idle", "dead"). A task matches
  // when its DISPLAY state — primary session's live status, or "pending" when
  // no live session is known — is in the set. Empty = no filter.
  repeated string states = 4;
  // 1-based page over the filtered, most-recently-active-first ordering.
  // page_size 0 = unpaginated (legacy callers); clamped to 1000 otherwise
  // (raised from 200 in rev 4: the web paginates by a growing page_size,
  // and a low clamp would silently stall its infinite scroll).
  int32 page = 5;
  int32 page_size = 6;
}
message ListTasksResponse {
  repeated Task tasks = 1;
  int32 total_count = 2; // matches BEFORE pagination
}
```

An empty request is bit-for-bit today's behavior, so existing callers (CLI
`task list`, web Fleet / operator Overview) are untouched.

### Where each filter runs (the SQL / in-memory split)

Session liveness is control-plane state joined in memory at read time (module
JSDoc in `rpc/tasks.ts`); it is not in the orchestrator's Postgres and must not
be denormalized there (the coordinator is the authority). So a pure
Drizzle-built query cannot express the state filter or the last-active sort.
Rather than split one filter pipeline across two engines, each concern runs
where its data lives:

- **SQL (Drizzle `where`)** — ownership scoping (`eq(createdByUserId, caller)`
  for mine-scope), the owner filter (`inArray`/`isNull` OR-ed), and (rev 3)
  the free-text search — `ilike` over title and task id plus an `exists`
  subquery over `task_session.session_id`, LIKE wildcards escaped. Everything
  expressible over Postgres data runs in Postgres; rows outside the caller's
  scope or search never leave the DB.
- **In-memory (handler)** — the state filter, last-active sort, and the page
  slice, over the already-scoped rows joined with live session state; plus
  the search match for synthetic unattributed rows only, which have no DB
  representation to query.

At the current fleet size (hundreds of tasks) this is strictly cheaper than
today's load-everything handler. If the table ever outgrows in-memory
filtering, the seam is the state filter: it would move to a coordinator-side
list filter (states are already in coord PG), after which search/sort/slice
push down into SQL. That is a follow-up, not this ADR.

The per-row CASL `can("read", …)` check stays as belt-and-braces under the SQL
scoping.

### Web surfaces

- **Sessions rail** = *all my tasks*: `scope=mine`, a search box, initial page
  of 25 growing by infinite scroll (rev 4: a shared `useLoadMoreSentinel`
  IntersectionObserver hook replaces every click-to-page affordance; each
  surface keeps ONE 1s-polled query whose `page_size` grows — deliberately not
  `useInfiniteQuery` page-appends, which would refetch every loaded page per
  poll tick). Rail filter state (search + visible count) lives in a small
  shared store so the ⌥-jump keymap and the command menu — which read the same
  `useRailSessions` rows — stay in lockstep with what the rail displays. The
  footer "My tasks" link is retired (the rail *is* my tasks).
- **`/sessions/list` (My tasks)** stays as the full-page/mobile rendering of
  the same searchable list (the rail is desktop-only), with a pager. The
  Live/Archived/All tabs are retired — recency sort + search + the admin state
  filter subsume them.
- **`/sessions/all` (All tasks, admin)** becomes the filter page: a search box
  plus ONE composite filter component (rev 2) in the reui/Linear shape — a
  single "Filter" trigger whose popover drills field → values, with each
  applied field rendered as a removable pill. The component (`filter-bar.tsx`)
  is generic and controlled (`fields` config + `value`/`onChange`); the page
  supplies the owner and session-state fields. Semantics: OR within a field,
  AND across fields — which is why the owner filter is `repeated` on the wire
  rather than a single id. Rows load by infinite scroll (50-per-step growing
  `page_size`) under a pinned heading + toolbar — only the virtualized list
  scrolls (rev 5, `@tanstack/react-virtual`). All filters are request params —
  the server does the work; the page renders what it's given.
- The start screen's recent-tasks preview switches to `scope=mine` (an admin's
  "recent" should be their own work).

## Consequences

- Admins' personal surfaces stop showing other users' tasks; the fleet-wide
  view is an explicit, admin-gated `scope=all`.
- `ListTasks` becomes the one list endpoint with real query semantics; the
  client-side tab filtering in `sessions-list.tsx` is deleted, not wrapped.
- First pagination/search precedent in the orchestrator: offset pages +
  `total_count`, SQL for scoping, in-memory for live-state-dependent filters.
- The 1s poll now moves one page, not the fleet.
