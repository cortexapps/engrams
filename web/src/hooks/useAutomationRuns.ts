import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  getRun,
  listRuns,
  retryRun,
  stopRun,
} from "@/gen/engram/app/v1/automation-AutomationRunService_connectquery";
import { isActiveRunStatus } from "@/pages/settings/automations/runs/run-format";

export interface RunListOptions {
  includeFiltered?: boolean;
  limit?: number;
}

/** The run list for one automation (ADR 0119 phase 3.7). Named apart from the
 * legacy `useAutomationRuns` in useAutomations.ts, which the 3.2 list page
 * retires with the old editor. */
export function useRunList(automationId: string | undefined, options: RunListOptions = {}) {
  return useQuery(
    listRuns,
    {
      automationId: automationId ?? "",
      limit: options.limit ?? 50,
      includeFiltered: options.includeFiltered ?? false,
    },
    { enabled: !!automationId, staleTime: 5_000 },
  );
}

const ACTIVE_POLL_MS = 2_000;

/** One run with its step ledger; polls while the run is still moving. */
export function useRun(runId: string | undefined) {
  return useQuery(
    getRun,
    { runId: runId ?? "" },
    {
      enabled: !!runId,
      refetchInterval: (query) => {
        const status = query.state.data?.run?.brief?.status;
        return status !== undefined && isActiveRunStatus(status) ? ACTIVE_POLL_MS : false;
      },
    },
  );
}

function useInvalidateRuns() {
  const queryClient = useQueryClient();
  return () =>
    Promise.all([
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listRuns, cardinality: "finite" }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getRun, cardinality: "finite" }),
      }),
    ]);
}

export function useStopRun() {
  const invalidate = useInvalidateRuns();
  return useMutation(stopRun, { onSuccess: invalidate });
}

export function useRetryRun() {
  const invalidate = useInvalidateRuns();
  return useMutation(retryRun, { onSuccess: invalidate });
}
