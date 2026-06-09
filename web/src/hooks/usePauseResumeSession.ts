import { useMutation, useQueryClient } from "@tanstack/react-query";
import { pauseSession, resumeSession } from "../api";

// ADR 0045 Phase F: freeze / unfreeze the session's microVM in place — the
// admin test surface for the pause/flush path. Unlike teleport, this does
// NOT change session state (the row stays `active`), so there's nothing to
// optimistically flip; we just invalidate on settle so any derived view
// refreshes. Returns both mutations from one hook (they're a pair).
export function usePauseResumeSession(sessionId: string) {
  const qc = useQueryClient();
  const invalidate = () => qc.invalidateQueries({ queryKey: ["session", sessionId] });

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
