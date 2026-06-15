import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import { drainHost, listHosts } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import type { ListHostsResponse } from "../gen/engram/app/v1/fleet_pb";

// Cordon a host from the Fleet surface. Optimistic: the host flips to
// `draining` (and its running-sandbox / capacity figures zero out)
// immediately in the connect-query listHosts cache so the strata redraw
// without waiting on the 1s poll; on error we roll back, and we always
// invalidate on settle so the next heartbeat is authoritative.

export function useDrainHost() {
  const qc = useQueryClient();
  const hostsKey = createConnectQueryKey({ schema: listHosts, input: {}, cardinality: "finite" });
  return useMutation(drainHost, {
    onMutate: async (req) => {
      const hostId = req.hostId ?? "";
      await qc.cancelQueries({ queryKey: hostsKey });
      const previous = qc.getQueryData<ListHostsResponse>(hostsKey);
      qc.setQueryData<ListHostsResponse>(hostsKey, (old) => {
        if (!old) return old;
        return {
          ...old,
          hosts: old.hosts.map((h) =>
            h.id === hostId
              ? { ...h, status: "draining", runningSandboxes: 0, capacityUsedMib: 0n }
              : h,
          ),
        };
      });
      return { previous };
    },
    onError: (_err, _req, ctx) => {
      if (ctx?.previous) qc.setQueryData(hostsKey, ctx.previous);
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: hostsKey });
    },
  });
}
