import {
  createConnectQueryKey,
  createQueryOptions,
  useMutation,
  useQuery,
  useTransport,
} from "@connectrpc/connect-query";
import { useQueries, useQueryClient } from "@tanstack/react-query";

import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";

import {
  getRun,
  listRuns,
  retryRun,
  stopRun,
} from "@/gen/engram/app/v1/automation-AutomationRunService_connectquery";
import { isActiveRunStatus } from "@/pages/automations/runs/run-format";

export interface RunListOptions {
  includeFiltered?: boolean;
  limit?: number;
  /** ADR 0120: only runs bound to this workstream. */
  instanceId?: string;
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
      ...(options.instanceId !== undefined ? { instanceId: options.instanceId } : {}),
    },
    { enabled: !!automationId, staleTime: 5_000 },
  );
}

/** Every automation's runs in one ledger, newest first. The service lists
 * per automation, so this fans out one query per id and merges; a window
 * carries its automation id so the ledger can name it. */
export function useAllRuns(
  automationIds: readonly string[],
  options: { includeFiltered?: boolean; limit?: number } = {},
): {
  runs: AutomationRunBrief[];
  windows: (FilteredWindow & { automationId: string })[];
  isPending: boolean;
  error: unknown;
} {
  const transport = useTransport();
  const results = useQueries({
    queries: automationIds.map((automationId) => ({
      ...createQueryOptions(
        listRuns,
        {
          automationId,
          limit: options.limit ?? 25,
          includeFiltered: options.includeFiltered ?? false,
        },
        { transport },
      ),
      staleTime: 5_000,
    })),
  });
  const runs = results
    .flatMap((r) => r.data?.runs ?? [])
    .sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt));
  const windows = results.flatMap((r, i) =>
    (r.data?.filtered ?? []).map((w) => ({ ...w, automationId: automationIds[i]! })),
  );
  return {
    runs,
    windows,
    isPending: results.some((r) => r.isPending),
    error: results.find((r) => r.error)?.error,
  };
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
        queryKey: createConnectQueryKey({
          schema: listRuns,
          cardinality: "finite",
        }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: getRun,
          cardinality: "finite",
        }),
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
