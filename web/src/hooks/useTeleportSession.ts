import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey } from "@connectrpc/connect-query";
import { teleportSession } from "../api";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { GetSessionResponse } from "../gen/engram/app/v1/session_pb";

// ADR 0045 Phase F: teleport an Active session to a chosen host. Optimistic:
// the session flips to `evacuating` immediately in the connect-query getSession
// cache (Task 24 migrated useSession to connect-query, so the legacy
// ["session", id] key is a silent no-op — retargeted here per review fix)
// so the status rail redraws without waiting on the next poll/event; on error
// we roll back, and we always invalidate on settle so the SSE/poll view is
// authoritative as the `evacuating → created → active` chain lands on the
// target.
export function useTeleportSession(sessionId: string) {
  const qc = useQueryClient();
  const sessionKey = createConnectQueryKey({
    schema: getSession,
    input: { sessionId },
    cardinality: "finite",
  });
  return useMutation({
    mutationFn: (targetHostId: string) => teleportSession(sessionId, targetHostId),
    onMutate: async () => {
      await qc.cancelQueries({ queryKey: sessionKey });
      const previous = qc.getQueryData<GetSessionResponse>(sessionKey);
      qc.setQueryData<GetSessionResponse>(sessionKey, (old) =>
        old?.session ? { ...old, session: { ...old.session, status: "evacuating" } } : old,
      );
      return { previous };
    },
    onError: (_err, _targetHostId, ctx) => {
      if (ctx?.previous) qc.setQueryData(sessionKey, ctx.previous);
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: sessionKey });
    },
  });
}
