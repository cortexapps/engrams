import { useQuery } from "@connectrpc/connect-query";
import { listTasks } from "../gen/engram/app/v1/task-TaskService_connectquery";
import type { SessionListItem } from "../types";
import type { Task } from "../gen/engram/app/v1/task_pb";

/** ADR 0039 Task 23: map a Task proto to the SessionListItem shape the list
 * components consume. A chat task has sessions[0] as its primary session.
 * `data-session-id` and navigation still point at the SESSION id — the detail
 * page is unchanged until Task 24. */
export function taskToSessionListItem(task: Task): SessionListItem {
  const ref = task.sessions[0];
  const sess = ref?.session;
  // Unattributed rows: id starts with 'unattributed-'; the session id is the
  // stable identifier. Fall back to task.id when there's no session ref yet.
  const id = ref?.sessionId ?? task.id;
  return {
    id,
    // Session proto fields are strings (proto3 generated TS), matching SessionState/SessionMode.
    status: (sess?.status ?? "pending") as SessionListItem["status"],
    image: sess?.image ?? "",
    mode: (sess?.mode ?? "agent") as SessionListItem["mode"],
    user_id: null,
    host_id: sess?.hostId ?? null,
    sandbox_id: sess?.sandboxId ?? null,
    created_at: sess?.createdAt ?? task.createdAt,
    last_active_at: sess?.lastActiveAt ?? task.createdAt,
    owner_email: null,
    owner_name: null,
    owner_kind: null,
  };
}

/** ADR 0039 Task 23: replaces useSessions for list surfaces.
 * ListTasks is server-scoped by the CASL ability — admin sees all, member
 * sees own. No scope param needed (the server handles it).
 *
 * Carry useSessions' exact TanStack options to preserve the 1s live-poll
 * behaviour and stable list under background refetches. */
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
