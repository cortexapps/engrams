import { useQuery } from '@tanstack/react-query';
import { fetchHarnesses } from '../api';

// Harnesses live above images — the host's `cfg.harnesses_dir`
// drives this list, deployment-wide. Same refresh shape as
// `useImages`: refetch on focus, 30s stale window. Operators add a
// new harness binary by dropping it on the host and restarting
// the coord, so polling beyond focus isn't useful.
export function useHarnesses(enabled: boolean) {
  return useQuery({
    queryKey: ['harnesses'],
    queryFn: fetchHarnesses,
    enabled,
    refetchOnWindowFocus: true,
    staleTime: 30_000,
  });
}
