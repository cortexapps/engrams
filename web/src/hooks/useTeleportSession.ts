import { useMutation, useQueryClient } from "@tanstack/react-query";
import { teleportSession } from "../api";
import type { Session } from "../types";

// ADR 0045 Phase F: teleport an Active session to a chosen host. Optimistic:
// the session flips to `evacuating` immediately in the ["session", id] cache
// so the status rail redraws without waiting on the next poll/event; on error
// we roll back, and we always invalidate on settle so the SSE/poll view is
// authoritative as the `evacuating → created → active` chain lands on the
// target.
export function useTeleportSession(sessionId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (targetHostId: string) => teleportSession(sessionId, targetHostId),
    onMutate: async () => {
      await qc.cancelQueries({ queryKey: ["session", sessionId] });
      const previous = qc.getQueryData<Session>(["session", sessionId]);
      qc.setQueryData<Session>(["session", sessionId], (old) =>
        old ? { ...old, status: "evacuating" } : old,
      );
      return { previous };
    },
    onError: (_err, _targetHostId, ctx) => {
      if (ctx?.previous) qc.setQueryData(["session", sessionId], ctx.previous);
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["session", sessionId] });
    },
  });
}
