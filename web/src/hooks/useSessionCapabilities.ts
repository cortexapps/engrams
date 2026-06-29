import { useQuery } from "@tanstack/react-query";

import { API_BASE } from "../lib/base";

export interface SessionCapabilities {
  /** Whether the in-guest browser (Xvfb + VNC) is available for this session (ADR 0064). */
  browserEnabled: boolean;
}

/**
 * GET /api/v1/sessions/:id/capabilities (ADR 0064, P2.3) — gates the BROWSER tab.
 *
 * A plain Hono route on the orchestrator (not a Connect/gRPC method), so this is a
 * bespoke `fetch` with the same `credentials: "include"` cookie auth the other
 * orchestrator REST hooks use (usePauseResumeSession, TokensPanel). A non-OK
 * response (404 against an older orchestrator that predates this route, or any
 * error) falls back to `{ browserEnabled: false }` so the tab simply stays hidden
 * rather than the query erroring — capability detection should degrade quietly.
 */
export function useSessionCapabilities(id: string | undefined) {
  return useQuery({
    queryKey: ["session-capabilities", id],
    enabled: !!id,
    staleTime: 60_000,
    queryFn: async (): Promise<SessionCapabilities> => {
      const res = await fetch(`${API_BASE}/sessions/${encodeURIComponent(id!)}/capabilities`, {
        credentials: "include",
      });
      if (!res.ok) return { browserEnabled: false };
      return res.json();
    },
  });
}
