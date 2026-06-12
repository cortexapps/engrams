import { useQuery } from "@connectrpc/connect-query";
import { useQuery as useTanstackQuery } from "@tanstack/react-query";
import { fetchSessions } from "../api";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { Session as ProtoSession } from "../gen/engram/app/v1/session_pb";
import type { Session } from "../types";

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

/** ADR 0031: owner-scoped. Members omit `scope` (their own); admins pass
 * `'all'` for the fleet-wide view. The scope is part of the query key so the
 * two views cache independently. */
export function useSessions(scope?: "mine" | "all") {
  return useTanstackQuery({
    queryKey: ["sessions", scope ?? "mine"],
    queryFn: () => fetchSessions(scope),
    refetchInterval: 1000,
    refetchOnWindowFocus: false,
    staleTime: 0,
    placeholderData: (prev) => prev,
  });
}

/** ADR 0039 Task 24: migrated to connect-query via the gated passthrough
 * (GetSession). Returns the legacy Session shape via protoSessionToSession so
 * all consumers (SessionDetail, CowState, useRailSessions) are unchanged.
 * Poll interval carried from the original 2s. */
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
