import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey } from "@connectrpc/connect-query";
import { pauseSession, resumeSession } from "../api";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";

// ADR 0045 Phase F: freeze / unfreeze the session's microVM in place — the
// admin test surface for the pause/flush path. Unlike teleport, this does
// NOT change session state (the row stays `active`), so there's nothing to
// optimistically flip; we just invalidate on settle so any derived view
// refreshes. Returns both mutations from one hook (they're a pair).
// (Task 24 migrated useSession to connect-query, so the legacy ["session", id]
// key is a silent no-op — retargeted here per review fix.)
export function usePauseResumeSession(sessionId: string) {
  const qc = useQueryClient();
  const sessionKey = createConnectQueryKey({
    schema: getSession,
    input: { sessionId },
    cardinality: "finite",
  });
  const invalidate = () => qc.invalidateQueries({ queryKey: sessionKey });

  const pause = useMutation({
    mutationFn: () => pauseSession(sessionId),
    onSettled: invalidate,
  });
  const resume = useMutation({
    mutationFn: () => resumeSession(sessionId),
    onSettled: invalidate,
  });

  return { pause, resume };
}
