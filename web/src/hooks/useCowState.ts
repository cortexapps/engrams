import { useQuery } from '@tanstack/react-query';
import { fetchHostCowState, fetchSessionCowState } from '../api';

// ADR 0016 Phase A: COW diagnostic data.
//
// Both endpoints are cached at coord with a 1s TTL, so polling at 2s
// keeps the freshness budget comfortable while avoiding a per-poll
// host RPC. The web app's display doesn't move on sub-second
// timescales — dirty-bytes accumulation is human-paced — so 2s is
// the sweet spot between "feels live" and "doesn't hammer the host".
const POLL_INTERVAL_MS = 2_000;

export function useHostCowState(hostId: string | undefined) {
  return useQuery({
    queryKey: ['cow-state', 'host', hostId],
    queryFn: () => fetchHostCowState(hostId!),
    enabled: hostId !== undefined,
    refetchInterval: POLL_INTERVAL_MS,
    // The data is purely diagnostic; stale renders for ≤2s while a
    // refetch is in flight are fine. Don't gate on focus — operators
    // leave this open in a side tab and want it to keep moving.
    refetchOnWindowFocus: false,
  });
}

export function useSessionCowState(sessionId: string | undefined) {
  return useQuery({
    queryKey: ['cow-state', 'session', sessionId],
    queryFn: () => fetchSessionCowState(sessionId!),
    enabled: sessionId !== undefined,
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: false,
  });
}
