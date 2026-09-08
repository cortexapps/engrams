/** Workstream (ADR 0120 instance) queries + lifecycle mutations. */

import {
  createConnectQueryKey,
  createQueryOptions,
  useMutation,
  useQuery,
  useTransport,
} from "@connectrpc/connect-query";
import { useQueries, useQueryClient } from "@tanstack/react-query";

import type { AutomationDropBrief, AutomationInstance } from "@/gen/engram/app/v1/automation_pb";

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

/** Every automation's workstreams in one list (the service lists per
 * automation). Open ones first, then by opened-at, newest first. */
export function useAllInstances(
  automationIds: readonly string[],
  options: { includeClosed?: boolean } = {},
): { instances: AutomationInstance[]; isPending: boolean; error: unknown } {
  const transport = useTransport();
  const results = useQueries({
    queries: automationIds.map((automationId) => ({
      ...createQueryOptions(
        listInstances,
        { automationId, includeClosed: options.includeClosed ?? false, limit: 0 },
        { transport },
      ),
      staleTime: 5_000,
    })),
  });
  const instances = results
    .flatMap((r) => r.data?.instances ?? [])
    .sort(
      (a, b) =>
        Number(b.status === "open") - Number(a.status === "open") ||
        Date.parse(b.openedAt) - Date.parse(a.openedAt),
    );
  return {
    instances,
    isPending: results.some((r) => r.isPending),
    error: results.find((r) => r.error)?.error,
  };
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

/** Recent routing misses across automations, annotated with their source. */
export function useAllRecentDrops(automationIds: readonly string[]): {
  drops: (AutomationDropBrief & { automationId: string })[];
  isPending: boolean;
  error: unknown;
} {
  const transport = useTransport();
  const results = useQueries({
    queries: automationIds.map((automationId) => ({
      ...createQueryOptions(listRecentDrops, { automationId, limit: 0 }, { transport }),
      staleTime: 5_000,
    })),
  });
  return {
    drops: results.flatMap((result, index) =>
      (result.data?.drops ?? []).map((drop) => ({
        ...drop,
        automationId: automationIds[index]!,
      })),
    ),
    isPending: results.some((result) => result.isPending),
    error: results.find((result) => result.error)?.error,
  };
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
