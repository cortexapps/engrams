import { useQuery } from '@tanstack/react-query';
import { fetchStorageSummary } from '../api';

// ADR 0029: the Storage surface's data. The endpoint aggregates the
// per-host COW state (coord-cached at 1s) plus two cheap Postgres
// counts, so it's safe to poll — but durability numbers move on a
// human, snapshot-cadence timescale, so 4s keeps it live-feeling
// without fanning host RPCs faster than they refresh.
const POLL_INTERVAL_MS = 4_000;

export function useStorageSummary() {
  return useQuery({
    queryKey: ['storage-summary'],
    queryFn: fetchStorageSummary,
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: false,
    staleTime: 0,
    placeholderData: (prev) => prev,
  });
}
