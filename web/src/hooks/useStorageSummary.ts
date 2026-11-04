import { useQuery } from "@connectrpc/connect-query";
import { getStorageSummary } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import type { StorageSummaryResponse } from "../lib/types";
import type { GetStorageSummaryResponse } from "../gen/engram/app/v1/fleet_pb";

// Two Postgres counts; cheap to poll, and they move on a human timescale.
const POLL_INTERVAL_MS = 4_000;

function protoStorageSummaryToLegacy(r: GetStorageSummaryResponse): StorageSummaryResponse {
  return {
    snapshots: Number(r.snapshots),
    snapshot_bytes: Number(r.snapshotBytes),
    gc_pending: Number(r.gcPending),
    gc_pending_exact: r.gcPendingExact,
  };
}

export function useStorageSummary(intervalMs = POLL_INTERVAL_MS) {
  return useQuery(
    getStorageSummary,
    {},
    {
      select: protoStorageSummaryToLegacy,
      refetchInterval: intervalMs,
      refetchOnWindowFocus: false,
      staleTime: 0,
      placeholderData: (prev) => prev,
    },
  );
}
