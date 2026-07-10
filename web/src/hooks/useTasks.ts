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
import { listTasks, updateTask } from "../gen/engram/app/v1/task-TaskService_connectquery";
import type { SessionListItem } from "../lib/types";
import type { Task } from "../gen/engram/app/v1/task_pb";
import { authClient } from "../lib/auth-client";

export interface TaskListParams {
  scope?: "" | "mine" | "all";
  search?: string;
  states?: string[];
  createdByUserIds?: string[];
  page?: number;
  pageSize?: number;
}

const TRANSITIONAL_TASK_STATES = new Set([
  "pending",
  "queued",
  "created",
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
 * `createdByUserId` is preserved in `user_id` so the AllSessions owner-enrichment
 * pass can look it up in the admin user map. owner_email/owner_name start null
 * and are filled by `enrichWithOwners` after the admin user list resolves. */
export function taskToSessionListItem(task: Task): SessionListItem {
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
    status: (sess?.status ?? "pending") as SessionListItem["status"],
    image: sess?.image ?? "",
    mode: (sess?.mode ?? "agent") as SessionListItem["mode"],
    // Carry createdByUserId through as user_id so owner-enrichment can look it
    // up in the admin user map. Null for unattributed/synthetic rows.
    user_id: task.createdByUserId ?? null,
    host_id: sess?.hostId ?? null,
    sandbox_id: sess?.sandboxId ?? null,
    created_at: sess?.createdAt ?? task.createdAt,
    last_active_at: sess?.lastActiveAt ?? task.createdAt,
    owner_email: null,
    owner_name: null,
    owner_kind: null,
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

/** Admin-only: fetch a stable id→{name,email} map from better-auth's admin
 * user list for owner-label enrichment on AllSessions. Fetches once with a
 * 60s stale window (admin list doesn't change frequently).
 *
 * Passes limit: 100 — sufficient for typical deployments; expand or paginate
 * if the fleet grows beyond a few dozen operators. */
async function fetchAdminUsersMap(): Promise<Map<string, { name: string; email: string }>> {
  // authClient.admin.listUsers returns { data: { users, total, ... } | null, error }
  const result = await authClient.admin.listUsers({ query: { limit: 100 } });
  const users =
    (result.data as { users?: Array<{ id: string; name: string; email: string }> } | null)?.users ??
    [];
  const map = new Map<string, { name: string; email: string }>();
  for (const u of users) {
    map.set(u.id, { name: u.name, email: u.email });
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
    // Keep the previous map on screen while refetching — avoids a "?" flash.
    placeholderData: (prev) => prev,
  });
}

/** Enrich a SessionListItem[] with owner_name/owner_email from the admin user
 * map. Rows with no user_id (unattributed/synthetic) keep null — "?" is honest. */
function enrichWithOwners(
  items: SessionListItem[],
  usersMap: Map<string, { name: string; email: string }> | undefined,
): SessionListItem[] {
  if (!usersMap) return items;
  return items.map((item) => {
    if (!item.user_id) return item;
    const u = usersMap.get(item.user_id);
    if (!u) return item;
    return { ...item, owner_name: u.name || null, owner_email: u.email };
  });
}

interface InfiniteSessionListResult {
  data: SessionListItem[] | undefined;
  totalCount: number | undefined;
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
  const data = query.data?.pages
    .flatMap((page) => page.tasks.map(taskToSessionListItem))
    .filter((item) => {
      if (seen.has(item.id)) return false;
      seen.add(item.id);
      return true;
    });
  const lastPage = query.data?.pages[query.data.pages.length - 1];

  return {
    data,
    totalCount: lastPage?.totalCount,
    hasNextPage: query.hasNextPage,
    isFetchingNextPage: query.isFetchingNextPage,
    fetchNextPage: query.fetchNextPage,
    isPending: query.isPending,
    error: query.error,
  };
}

/** Admin variant of the infinite task list with owner labels resolved from the
 * better-auth admin user map. */
export function useTasksInfiniteAsSessionListWithOwners(
  params: Omit<TaskListParams, "page" | "pageSize">,
  pageSize: number,
): InfiniteSessionListResult {
  const result = useTasksInfiniteAsSessionList(params, pageSize);
  const { data: usersMap } = useAdminUsersMap(true);
  return {
    ...result,
    data: result.data ? enrichWithOwners(result.data, usersMap) : undefined,
  };
}

/** Convert the ListTasksResponse tasks array to SessionListItem[]. */
export function useTasksAsSessionList(params?: TaskListParams): {
  data: SessionListItem[] | undefined;
  totalCount: number | undefined;
  isPending: boolean;
  error: unknown;
} {
  const { data, isPending, error } = useTasks(params);
  return {
    data: data?.tasks.map(taskToSessionListItem),
    totalCount: data?.totalCount,
    isPending,
    error,
  };
}

/** Admin variant: same as useTasksAsSessionList but enriches owner fields
 * from the better-auth admin user list. Used exclusively by AllSessions. */
export function useTasksAsSessionListWithOwners(params?: TaskListParams): {
  data: SessionListItem[] | undefined;
  totalCount: number | undefined;
  isPending: boolean;
  error: unknown;
} {
  const { data, isPending, error } = useTasks(params);
  const { data: usersMap } = useAdminUsersMap(true);
  const raw = data?.tasks.map(taskToSessionListItem);
  return {
    data: raw ? enrichWithOwners(raw, usersMap) : undefined,
    totalCount: data?.totalCount,
    isPending,
    error,
  };
}
