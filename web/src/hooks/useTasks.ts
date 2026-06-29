import { useQuery } from "@connectrpc/connect-query";
import { useQuery as useTanstackQuery } from "@tanstack/react-query";
import { listTasks } from "../gen/engram/app/v1/task-TaskService_connectquery";
import type { SessionListItem } from "../lib/types";
import type { Task } from "../gen/engram/app/v1/task_pb";
import { authClient } from "../lib/auth-client";

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
  return {
    id,
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
 * ListTasks is server-scoped by the CASL ability — admin sees all, member
 * sees own. No scope param needed (the server handles it).
 *
 * Options are behaviorally equivalent to the former useSessions options
 * (refetchOnWindowFocus and staleTime come from TanStack defaults, not explicit
 * carry-over — 1s live-poll and placeholderData are the meaningful deltas). */
export function useTasks() {
  return useQuery(
    listTasks,
    {},
    {
      refetchInterval: 1_000,
      placeholderData: (prev) => prev,
    },
  );
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

/** Convert the ListTasksResponse tasks array to SessionListItem[]. */
export function useTasksAsSessionList(): {
  data: SessionListItem[] | undefined;
  isPending: boolean;
  error: unknown;
} {
  const { data, isPending, error } = useTasks();
  return {
    data: data?.tasks.map(taskToSessionListItem),
    isPending,
    error,
  };
}

/** Admin variant: same as useTasksAsSessionList but enriches owner fields
 * from the better-auth admin user list. Used exclusively by AllSessions. */
export function useTasksAsSessionListWithOwners(): {
  data: SessionListItem[] | undefined;
  isPending: boolean;
  error: unknown;
} {
  const { data, isPending, error } = useTasks();
  const { data: usersMap } = useAdminUsersMap(true);
  const raw = data?.tasks.map(taskToSessionListItem);
  return {
    data: raw ? enrichWithOwners(raw, usersMap) : undefined,
    isPending,
    error,
  };
}
