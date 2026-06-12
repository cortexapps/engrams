import { useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey, useMutation } from "@connectrpc/connect-query";
import { evacuateSession } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { GetSessionResponse } from "../gen/engram/app/v1/session_pb";

// ADR 0039 Task 28: migrated from REST POST /api/v1/admin/sessions/:id/teleport
// to FleetService.EvacuateSession (connect-query passthrough via the orchestrator).
// EvacuateSessionRequest accepts sessionId + optional targetHost; the coordinator
// scanner picks the target when targetHost is omitted, but the UI passes the
// admin-chosen host just as the old REST path did.
//
// ADR 0045 Phase F: teleport an Active session to a chosen host. Optimistic:
// the session flips to `evacuating` immediately in the connect-query getSession
// cache so the status rail redraws without waiting on the next poll/event; on
// error we roll back, and we always invalidate on settle so the SSE/poll view is
// authoritative as the `evacuating → created → active` chain lands on the target.
export function useTeleportSession(sessionId: string) {
  const qc = useQueryClient();
  const sessionKey = createConnectQueryKey({
    schema: getSession,
    input: { sessionId },
    cardinality: "finite",
  });
  // evacuateSession is FleetService.EvacuateSession — admin-gated at the
  // orchestrator CASL layer. The input shape takes sessionId + optional targetHost.
  const mutation = useMutation(evacuateSession, {
    onMutate: async () => {
      await qc.cancelQueries({ queryKey: sessionKey });
      const previous = qc.getQueryData<GetSessionResponse>(sessionKey);
      qc.setQueryData<GetSessionResponse>(sessionKey, (old) =>
        old?.session ? { ...old, session: { ...old.session, status: "evacuating" } } : old,
      );
      return { previous };
    },
    onError: (_err, _input, ctx) => {
      const rollback = (ctx as { previous?: GetSessionResponse } | undefined)?.previous;
      if (rollback) qc.setQueryData(sessionKey, rollback);
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: sessionKey });
    },
  });

  // Expose the same interface as before: mutate(targetHostId).
  return {
    mutate: (targetHostId: string) => mutation.mutate({ sessionId, targetHost: targetHostId }),
    mutateAsync: (targetHostId: string) =>
      mutation.mutateAsync({ sessionId, targetHost: targetHostId }),
    isPending: mutation.isPending,
    error: mutation.error,
    status: mutation.status,
  };
}
