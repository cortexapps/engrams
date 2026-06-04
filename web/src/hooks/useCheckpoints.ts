import { useQuery } from '@tanstack/react-query';
import { fetchSessionCheckpoints } from '../api';

// ADR 0028 A.log: the session's checkpoint chain feeds the durability
// timeline + chain list. Polls at 5s — checkpoints land on a
// minute-scale cadence, so this is plenty fresh and cheap (one indexed
// PG read per poll).
const POLL_INTERVAL_MS = 5_000;

export function useSessionCheckpoints(sessionId: string | undefined) {
  return useQuery({
    queryKey: ['checkpoints', 'session', sessionId],
    queryFn: () => fetchSessionCheckpoints(sessionId!),
    enabled: sessionId !== undefined,
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: false,
  });
}
