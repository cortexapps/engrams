import { useQuery } from '@tanstack/react-query';
import { fetchSession, fetchSessions } from '../api';

/** ADR 0031: owner-scoped. Members omit `scope` (their own); admins pass
 * `'all'` for the fleet-wide view. The scope is part of the query key so the
 * two views cache independently. */
export function useSessions(scope?: 'mine' | 'all') {
  return useQuery({
    queryKey: ['sessions', scope ?? 'mine'],
    queryFn: () => fetchSessions(scope),
    refetchInterval: 1000,
    refetchOnWindowFocus: false,
    staleTime: 0,
    placeholderData: (prev) => prev,
  });
}

export function useSession(id: string | undefined) {
  return useQuery({
    queryKey: ['session', id],
    queryFn: () => fetchSession(id!),
    enabled: !!id,
    refetchInterval: 2000,
    refetchOnWindowFocus: false,
    staleTime: 0,
    placeholderData: (prev) => prev,
  });
}
