import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey } from "@connectrpc/connect-query";
import { API_BASE } from "../lib/base";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";

// ADR 0051 Task 28: pause/resume POST /api/v1/admin/sessions/:id/{pause,resume} now
// route to the orchestrator admin proxy (routes/admin.ts) rather than the coordinator
// REST API directly. The URL path is unchanged; the orchestrator forwards with the
// service bearer. PauseSession/ResumeSession have no gRPC equivalent yet — kept as
// REST until a future task adds FleetService.PauseSession proto methods.

// ADR 0045 Phase F: freeze / unfreeze the session's microVM in place — the
// admin test surface for the pause/flush path. Unlike teleport, this does
// NOT change session state (the row stays `active`), so there's nothing to
// optimistically flip; we just invalidate on settle so any derived view
// refreshes. Returns both mutations from one hook (they're a pair).
export function usePauseResumeSession(sessionId: string) {
  const qc = useQueryClient();
  const sessionKey = createConnectQueryKey({
    schema: getSession,
    input: { sessionId },
    cardinality: "finite",
  });
  const invalidate = () => qc.invalidateQueries({ queryKey: sessionKey });

  const pause = useMutation({
    mutationFn: async () => {
      const res = await fetch(`${API_BASE}/admin/sessions/${encodeURIComponent(sessionId)}/pause`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: "{}",
      });
      if (!res.ok) throw new Error(`pause → ${res.status}`);
    },
    onSettled: invalidate,
  });
  const resume = useMutation({
    mutationFn: async () => {
      const res = await fetch(
        `${API_BASE}/admin/sessions/${encodeURIComponent(sessionId)}/resume`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: "{}",
        },
      );
      if (!res.ok) throw new Error(`resume → ${res.status}`);
    },
    onSettled: invalidate,
  });

  return { pause, resume };
}
