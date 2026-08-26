/** Workstream (ADR 0120 instance) queries + lifecycle mutations. */

import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  closeInstance,
  getInstance,
  listInstances,
  listRecentDrops,
  listRuns,
} from "@/gen/engram/app/v1/automation-AutomationRunService_connectquery";

export function useInstanceList(
  automationId: string | undefined,
  options: { includeClosed?: boolean; enabled?: boolean } = {},
) {
  return useQuery(
    listInstances,
    {
      automationId: automationId ?? "",
      includeClosed: options.includeClosed ?? false,
      limit: 0,
    },
    { enabled: (options.enabled ?? true) && !!automationId, staleTime: 5_000 },
  );
}

export function useInstance(id: string | undefined) {
  return useQuery(getInstance, { id: id ?? "" }, { enabled: !!id, staleTime: 5_000 });
}

export function useRecentDrops(automationId: string | undefined, enabled = true) {
  return useQuery(
    listRecentDrops,
    { automationId: automationId ?? "", limit: 0 },
    { enabled: enabled && !!automationId, staleTime: 5_000 },
  );
}

export function useCloseInstance() {
  const queryClient = useQueryClient();
  return useMutation(closeInstance, {
    onSuccess: () =>
      Promise.all([
        queryClient.invalidateQueries({
          queryKey: createConnectQueryKey({
            schema: listInstances,
            cardinality: "finite",
          }),
        }),
        queryClient.invalidateQueries({
          queryKey: createConnectQueryKey({
            schema: getInstance,
            cardinality: "finite",
          }),
        }),
        queryClient.invalidateQueries({
          queryKey: createConnectQueryKey({
            schema: listRuns,
            cardinality: "finite",
          }),
        }),
      ]),
  });
}

/** Invalidate workstream lists after a kickoff (RunNow with instance_key). */
export function useInvalidateInstances() {
  const queryClient = useQueryClient();
  return () =>
    queryClient.invalidateQueries({
      queryKey: createConnectQueryKey({
        schema: listInstances,
        cardinality: "finite",
      }),
    });
}
