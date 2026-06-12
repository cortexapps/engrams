import { useQuery } from "@connectrpc/connect-query";
import { getStorageSummary } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import type { StorageSummaryResponse, DurabilityRow } from "../lib/types";
import type {
  GetStorageSummaryResponse,
  DurabilityRow as ProtoDurabilityRow,
} from "../gen/engram/app/v1/fleet_pb";

// ADR 0029: the Storage surface's data. The endpoint aggregates the
// per-host COW state (coord-cached at 1s) plus two cheap Postgres
// counts, so it's safe to poll — but durability numbers move on a
// human, snapshot-cadence timescale, so 4s keeps it live-feeling
// without fanning host RPCs faster than they refresh.
const POLL_INTERVAL_MS = 4_000;

function protoDurabilityRowToLegacy(r: ProtoDurabilityRow): DurabilityRow {
  return {
    sandbox_id: r.sandboxId,
    session_id: r.sessionId ?? null,
    host_id: r.hostId,
    dirty_chunks: r.dirtyChunks,
    dirty_bytes: Number(r.dirtyBytes),
    base_chunks: r.baseChunks,
    base_chunks_local: r.baseChunksLocal,
    last_flush_at: r.lastFlushAt ?? null,
  };
}

function protoStorageSummaryToLegacy(r: GetStorageSummaryResponse): StorageSummaryResponse {
  return {
    snapshots: Number(r.snapshots),
    snapshot_bytes: Number(r.snapshotBytes),
    gc_pending: Number(r.gcPending),
    tracked_sandboxes: Number(r.trackedSandboxes),
    dirty_chunks: Number(r.dirtyChunks),
    unflushed_bytes: Number(r.unflushedBytes),
    avg_locality_pct: r.avgLocalityPct,
    rows: r.rows.map(protoDurabilityRowToLegacy),
  };
}

export function useStorageSummary() {
  return useQuery(
    getStorageSummary,
    {},
    {
      select: protoStorageSummaryToLegacy,
      refetchInterval: POLL_INTERVAL_MS,
      refetchOnWindowFocus: false,
      staleTime: 0,
      placeholderData: (prev) => prev,
    },
  );
}
