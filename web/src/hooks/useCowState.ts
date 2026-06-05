import { useQuery } from "@tanstack/react-query";
import { fetchSessionCowState } from "../api";

// ADR 0016 Phase A: per-session COW diagnostic data.
//
// The endpoint is cached at coord with a 1s TTL, so polling at 2s
// keeps the freshness budget comfortable while avoiding a per-poll
// host RPC. The web app's display doesn't move on sub-second
// timescales — dirty-bytes accumulation is human-paced — so 2s is
// the sweet spot between "feels live" and "doesn't hammer the host".
//
// ADR 0029: the host-wide COW view moved to the Storage surface, which
// reads the coordinator's aggregated storage-summary endpoint rather
// than fanning out per-host cow-state from the browser.
const POLL_INTERVAL_MS = 2_000;

export function useSessionCowState(sessionId: string | undefined) {
  return useQuery({
    queryKey: ["cow-state", "session", sessionId],
    queryFn: () => fetchSessionCowState(sessionId!),
    enabled: sessionId !== undefined,
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: false,
  });
}
