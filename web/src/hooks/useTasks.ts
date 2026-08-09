import {
  createConnectQueryKey,
  useInfiniteQuery,
  useMutation,
  useQuery,
} from "@connectrpc/connect-query";
import {
  keepPreviousData,
  useQuery as useTanstackQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  deleteTask,
  getTask,
  listTasks,
  updateTask,
} from "../gen/engram/app/v1/task-TaskService_connectquery";
import { UNKNOWN_STATE, type ListRowState, type SessionListItem } from "../lib/types";
import type { Task } from "../gen/engram/app/v1/task_pb";
import { authClient } from "../lib/auth-client";

export interface TaskListParams {
  scope?: "" | "mine" | "all";
  search?: string;
  states?: string[];
  createdByUserIds?: string[];
  page?: number;
  pageSize?: number;
  /** "" (default) bands live work onto page one; "recency" returns the newest
   *  first whatever their state. See `order` on ListTasksRequest. */
  order?: "" | "recency";
}

const TRANSITIONAL_TASK_STATES = new Set([
  "pending",
  "queued",
  "created",
  // Unreachable is mid-recovery the moment anything prompts/resumes it —
  // keep the fast poll so the recovery transition is seen promptly.
  "unreachable",
  "evacuating",
  "evicting",
  "host_lost",
]);

/** State changes are bursty around task boot and eviction, while ambient task
 * changes do not need sub-second freshness. Keep transitional work responsive
 * and let settled lists poll quietly; see ADR 0087.
 *
 * A task with NO live session (coordinator GC'd it) DISPLAYS as "pending"
 * (taskToSessionListItem's fallback) but is not transitional — nothing is
 * booting, and old datasets are full of such rows. Counting the fallback here
 * would pin every list at the fast interval forever, so only a status from a
 * real live session qualifies. */
export function pollIntervalFor(tasks: readonly Task[] | undefined): number {
  return tasks?.some((task) => {
    const status = task.sessions[0]?.session?.status;
    return status !== undefined && TRANSITIONAL_TASK_STATES.has(status);
  })
    ? 2_000
    : 30_000;
}

/** ADR 0051 Task 23: map a Task proto to the SessionListItem shape the list
 * components consume. A chat task has sessions[0] as its primary session.
 * `data-session-id` and navigation still point at the SESSION id — the detail
 * page is unchanged until Task 24.
 *
 * `createdByUserId` is preserved in `user_id` for filtering and authorization;
 * owner labels ride the Task row from the server-side identity join. */
export function taskToSessionListItem(
  task: Task,
  /** The INVERSE of `sessionStateUnavailable` on the response. False means the
   *  control plane did not answer, so an absent session is UNKNOWN, not gone. */
  sessionStateAvailable = true,
): SessionListItem {
  const ref = task.sessions[0];
  const sess = ref?.session;
  const snap = ref?.profile;
  // Unattributed rows: id starts with 'unattributed-'; the session id is the
  // stable identifier. Fall back to task.id when there's no session ref yet.
  const id = ref?.sessionId ?? task.id;
  // Synthetic admin rows (`unattributed-<sessionId>`) have no real task row —
  // taskId null so the UI hides the rename control (UpdateTask would 404).
  const taskId = task.id.startsWith("unattributed-") ? null : task.id;
  return {
    id,
    taskId,
    title: task.title ?? null,
    titleIsCustom: task.titleIsCustom,
    // Session proto fields are strings (proto3 generated TS), matching SessionState/SessionMode.
    //
    // A ref carrying a session id but NO session means the control plane no
    // longer has it: the sandbox was collected, which is `dead`. NOT `pending`,
    // the very first state of the lifecycle — that would draw a task reaped
    // months ago as if it were about to start, and band it under "Working".
    // Only a task with no reference at all is genuinely pending.
    //
    // Unless the control plane never answered, in which case EVERY session is
    // absent and none of them is dead — `unknown`, and the surfaces stop
    // banding. Mirrors `displayState` in orchestrator/src/rpc/tasks.ts, which
    // makes the same call for the state filter and the list ordering; keep the
    // two in lockstep.
    status: (sess?.status ??
      (!ref ? "pending" : sessionStateAvailable ? "dead" : UNKNOWN_STATE)) as ListRowState,
    needsAttention: task.status === "awaiting_review",
    image: sess?.image ?? "",
    mode: (sess?.mode ?? "agent") as SessionListItem["mode"],
    // Carry the raw attribution fact through for list consumers. Null for
    // unattributed/synthetic rows.
    user_id: task.createdByUserId ?? null,
    host_id: sess?.hostId ?? null,
    sandbox_id: sess?.sandboxId ?? null,
    created_at: sess?.createdAt ?? task.createdAt,
    // The row's "last active" label must read the SAME clock the orchestrator
    // ordered this list by (`taskActivityAt` in orchestrator/src/rpc/tasks.ts):
    // the activity clock first, then the state-machine clock, then the task's
    // own createdAt. Reading `lastActiveAt` here while the server sorts on
    // `lastEventAt` would render a list ordered by one clock and labelled with
    // another — a row stamped "2 months ago" sitting above one stamped
    // "5 minutes ago".
    last_active_at: sess?.lastEventAt ?? sess?.lastActiveAt ?? task.createdAt,
    owner_email: task.createdBy?.email ?? null,
    owner_name: task.createdBy?.name ?? null,
    owner_kind: task.createdByUserId == null ? "system" : null,
    profile: snap
      ? {
          id: snap.id,
          name: snap.name,
          icon: snap.icon,
          archived: snap.archived,
          imageUri: snap.imageUri,
          skills: snap.skills,
        }
      : null,
  };
}

/** ADR 0051 Task 23: replaces useSessions for list surfaces.
 * ListTasks defaults to the legacy CASL-ability scope, while explicit params
 * narrow personal/admin surfaces and enable server-side filtering/pagination.
 *
 * Options are behaviorally equivalent to the former useSessions options while
 * adapting polling frequency to the freshest task state. */
export function useTasks(params?: TaskListParams) {
  return useQuery(listTasks, params ?? {}, {
    refetchInterval: (query) => pollIntervalFor(query.state.data?.tasks),
    refetchOnWindowFocus: true,
    placeholderData: keepPreviousData,
  });
}

/** Fetch one task with its complete descendant tree (ADR 0113). */
export function useTask(id: string | null | undefined) {
  return useQuery(
    getTask,
    { taskId: id ?? "" },
    {
      enabled: !!id,
      refetchInterval: 2_000,
      refetchOnWindowFocus: true,
      select: (response) => response.task,
    },
  );
}

/** Resolve the task that owns a primary session, including hidden child tasks. */
export function useTaskForSession(sessionId: string | null | undefined) {
  return useQuery(
    getTask,
    { sessionId: sessionId ?? "" },
    {
      enabled: !!sessionId,
      refetchInterval: 2_000,
      refetchOnWindowFocus: true,
      select: (response) => response.task,
    },
  );
}

/**
 * Rename (or reset) a task's title. Pass `{ taskId, title }` to set a sticky
 * custom title, or `{ taskId }` (title omitted) to reset to the auto title.
 * Invalidates the task list so every surface re-renders with the new title.
 */
export function useUpdateTask() {
  const qc = useQueryClient();
  return useMutation(updateTask, {
    // cardinality undefined → the whole ListTasks key family, finite AND
    // infinite: the list surfaces paginate with useInfiniteQuery (ADR 0087),
    // and a finite-only invalidation would leave their titles stale until the
    // next ambient poll.
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: undefined }),
      }),
  });
}

/**
 * Delete a task and tear down its sessions. DeleteTask calls DeleteSession for
 * each task_session row (the racy-idempotent teardown; ADR 0051/0079) and then
 * removes the task row, so the deleted work drops out of every list surface.
 * Invalidates the whole ListTasks key family (finite AND infinite, per
 * useUpdateTask's note) so the list re-renders without the removed row.
 */
export function useDeleteTask() {
  const qc = useQueryClient();
  return useMutation(deleteTask, {
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: undefined }),
      }),
  });
}

/** Admin-only: fetch a stable id→{name,email} map from better-auth's admin
 * user list for the AllSessions owner-filter options. Fetches once with a
 * 60s stale window (admin list doesn't change frequently).
 *
 * Pages through the WHOLE directory: a single capped fetch left any owner
 * past the first page unmapped — dev/e2e databases accumulate hundreds of
 * users, and real owners sorted after them rendered as "?". */
async function fetchAdminUsersMap(): Promise<Map<string, { name: string; email: string }>> {
  const map = new Map<string, { name: string; email: string }>();
  const PAGE = 100;
  let offset = 0;
  for (;;) {
    // authClient.admin.listUsers returns { data: { users, total, ... } | null, error }
    const result = await authClient.admin.listUsers({ query: { limit: PAGE, offset } });
    const data = result.data as {
      users?: Array<{ id: string; name: string; email: string }>;
      total?: number;
    } | null;
    const users = data?.users ?? [];
    for (const u of users) {
      map.set(u.id, { name: u.name, email: u.email });
    }
    offset += users.length;
    // Terminates even if the server ignores `offset` (users.length stalls the
    // running offset at total) or omits `total` (falls back to one page).
    if (users.length === 0 || offset >= (data?.total ?? offset)) break;
  }
  return map;
}

/** Fetch the admin user map (id → name/email). Only enabled when
 * `isAdmin` is true so member-scoped pages never call the admin endpoint. */
export function useAdminUsersMap(isAdmin: boolean) {
  return useTanstackQuery({
    queryKey: ["admin", "users-map"],
    queryFn: fetchAdminUsersMap,
    enabled: isAdmin,
    staleTime: 60_000,
    // Keep filter options stable while the directory refetches.
    placeholderData: (prev) => prev,
  });
}

interface InfiniteSessionListResult {
  data: SessionListItem[] | undefined;
  totalCount: number | undefined;
  /** False when any loaded page came back without live session state. */
  sessionStateAvailable: boolean;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  fetchNextPage: ReturnType<typeof useInfiniteQuery>["fetchNextPage"];
  isPending: boolean;
  error: unknown;
}

/** Paginated ListTasks projected into the flat list shape used by session
 * surfaces. The installed connect-query v2 API derives its initial page param
 * from the required `page` field and removes that field from the cache key. */
export function useTasksInfiniteAsSessionList(
  params: Omit<TaskListParams, "page" | "pageSize">,
  pageSize: number,
): InfiniteSessionListResult {
  const query = useInfiniteQuery(
    listTasks,
    { ...params, pageSize, page: 1 },
    {
      pageParamKey: "page",
      getNextPageParam: (lastPage, allPages) => {
        const rowsSoFar = allPages.reduce((count, page) => count + page.tasks.length, 0);
        return rowsSoFar < lastPage.totalCount ? allPages.length + 1 : undefined;
      },
      // An interval refetch re-requests EVERY loaded page (that full-chain
      // snapshot is load-bearing: it heals the transient row-drop/dup seams
      // offset pages tear under live reordering). Scaling the interval by the
      // loaded depth keeps total request volume constant no matter how deep a
      // viewer has scrolled — the common case (one page, watching a task
      // boot) keeps the fast cadence.
      refetchInterval: (currentQuery) => {
        const pages = currentQuery.state.data?.pages;
        return (
          pollIntervalFor(pages?.flatMap((page) => page.tasks)) * Math.max(1, pages?.length ?? 1)
        );
      },
      refetchOnWindowFocus: true,
      placeholderData: keepPreviousData,
    },
  );

  const seen = new Set<string>();
  // Live reordering can move the same task across page boundaries between
  // fetches, so keep the first occurrence to preserve server display order.
  // Per PAGE, not per response: a poll refetches every loaded page, so one of
  // them can come back without live state while the others have it.
  const data = query.data?.pages
    .flatMap((page) =>
      page.tasks.map((task) => taskToSessionListItem(task, !page.sessionStateUnavailable)),
    )
    .filter((item) => {
      if (seen.has(item.id)) return false;
      seen.add(item.id);
      return true;
    });
  const lastPage = query.data?.pages[query.data.pages.length - 1];

  return {
    data,
    totalCount: lastPage?.totalCount,
    // One bad page is enough: the surfaces stop banding rather than band a
    // list where some rows have live state and some are silently unknown.
    sessionStateAvailable: (query.data?.pages ?? []).every((page) => !page.sessionStateUnavailable),
    hasNextPage: query.hasNextPage,
    isFetchingNextPage: query.isFetchingNextPage,
    fetchNextPage: query.fetchNextPage,
    isPending: query.isPending,
    error: query.error,
  };
}

/** Convert the ListTasksResponse tasks array to SessionListItem[]. */
export function useTasksAsSessionList(params?: TaskListParams): {
  data: SessionListItem[] | undefined;
  totalCount: number | undefined;
  sessionStateAvailable: boolean;
  isPending: boolean;
  error: unknown;
} {
  const { data, isPending, error } = useTasks(params);
  const available = !data?.sessionStateUnavailable;
  return {
    data: data?.tasks.map((task) => taskToSessionListItem(task, available)),
    totalCount: data?.totalCount,
    sessionStateAvailable: available,
    isPending,
    error,
  };
}
