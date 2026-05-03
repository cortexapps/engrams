import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  addHarnessPack,
  deleteHarnessPack,
  fetchHarnessPacks,
} from '../api';
import type { AddHarnessPackRequest } from '../types';

// Distinct from `useHarnesses` (the session-create dropdown) by
// intent: this hook drives the settings panel's CRUD surface and
// invalidates on writes. Both hit `/api/harnesses` today; if the
// shapes diverge we add a `?settings` flag or a sibling endpoint.
const KEY = ['harness-packs'] as const;

export function useHarnessPacks() {
  return useQuery({
    queryKey: KEY,
    queryFn: fetchHarnessPacks,
    refetchOnWindowFocus: true,
    staleTime: 10_000,
  });
}

export function useAddHarnessPack() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: AddHarnessPackRequest) => addHarnessPack(req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: KEY });
      // Session-create form reads from `/api/harnesses` too — bust
      // its cache so a freshly-added harness shows up in the
      // dropdown immediately.
      qc.invalidateQueries({ queryKey: ['harnesses'] });
    },
  });
}

export function useDeleteHarnessPack() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => deleteHarnessPack(name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: KEY });
      qc.invalidateQueries({ queryKey: ['harnesses'] });
    },
  });
}
