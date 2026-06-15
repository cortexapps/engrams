import { useQuery } from "@connectrpc/connect-query";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { Session as ProtoSession } from "../gen/engram/app/v1/session_pb";
import type { Session } from "../lib/types";

/** ADR 0039 Task 24: map a proto Session to the legacy snake_case Session shape
 * that all consumers (SessionDetail, CowState, useRailSessions, etc.) expect.
 * Keeps consumers unchanged — the proto camelCase fields are converted here.
 * `user_id` is intentionally null: the proto Session omits attribution (ADR §2.1). */
function protoSessionToSession(s: ProtoSession): Session {
  return {
    id: s.id,
    user_id: null,
    status: s.status as Session["status"],
    host_id: s.hostId ?? null,
    sandbox_id: s.sandboxId ?? null,
    image: s.image,
    mode: s.mode as Session["mode"],
    created_at: s.createdAt,
    last_active_at: s.lastActiveAt,
  };
}

/** ADR 0039 Task 24: migrated to connect-query via the gated passthrough
 * (GetSession). Returns the legacy Session shape via protoSessionToSession so
 * all consumers (SessionDetail, CowState, useRailSessions) are unchanged.
 * Poll interval carried from the original 2s.
 *
 * ADR 0039 Task 28: useSessions (REST list) removed — use useTasksAsSessionList
 * (hooks/useTasks.ts) for list surfaces. The REST /api/v1/sessions route is no
 * longer reachable from the browser after the proxy flip. */
export function useSession(id: string | undefined) {
  return useQuery(
    getSession,
    { sessionId: id ?? "" },
    {
      enabled: !!id,
      refetchInterval: 2_000,
      refetchOnWindowFocus: false,
      staleTime: 0,
      placeholderData: (prev) => prev,
      select: (resp) => (resp.session ? protoSessionToSession(resp.session) : undefined),
    },
  );
}
