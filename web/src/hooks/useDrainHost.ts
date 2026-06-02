import { useMutation, useQueryClient } from '@tanstack/react-query';
import { drainHost } from '../api';
import type { HostView } from '../types';

// Cordon a host from the Fleet surface. Optimistic: the host flips to
// `draining` (and its running-sandbox / capacity figures zero out)
// immediately in the `['hosts']` cache so the strata redraw without
// waiting on the 1s poll; on error we roll back, and we always
// invalidate on settle so the next heartbeat is authoritative.

export function useDrainHost() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (hostId: string) => drainHost(hostId),
    onMutate: async (hostId: string) => {
      await qc.cancelQueries({ queryKey: ['hosts'] });
      const previous = qc.getQueryData<HostView[]>(['hosts']);
      qc.setQueryData<HostView[]>(['hosts'], (old) =>
        (old ?? []).map((h) =>
          h.id === hostId
            ? { ...h, status: 'draining', running_sandboxes: 0, capacity_used_mib: 0 }
            : h,
        ),
      );
      return { previous };
    },
    onError: (_err, _hostId, ctx) => {
      if (ctx?.previous) qc.setQueryData(['hosts'], ctx.previous);
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ['hosts'] });
    },
  });
}
