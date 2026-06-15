import { useQuery } from "@connectrpc/connect-query";
import { listCheckpoints } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { CheckpointSummary as ProtoCheckpointSummary } from "../gen/engram/app/v1/session_pb";
import type { CheckpointSummary, CheckpointsResponse } from "../lib/types";

// ADR 0028 A.log: the session's checkpoint chain feeds the durability
// timeline + chain list. Polls at 5s — checkpoints land on a
// minute-scale cadence, so this is plenty fresh and cheap (one indexed
// PG read per poll).
//
// ADR 0051 Task 24: migrated to connect-query via the gated passthrough
// (ListCheckpoints). Returns the legacy CheckpointsResponse shape via
// protoCheckpointToCheckpointSummary so DurabilityTimeline is unchanged.
const POLL_INTERVAL_MS = 5_000;

/** Map a proto CheckpointSummary (camelCase, bigint) to the legacy
 * snake_case shape. sizeBytes / eventsCursor are bigint in proto — convert
 * with Number() (sizes never exceed MAX_SAFE_INTEGER in practice). */
function protoCheckpointToCheckpointSummary(c: ProtoCheckpointSummary): CheckpointSummary {
  return {
    snapshot_id: c.snapshotId,
    created_at: c.createdAt,
    size_bytes: Number(c.sizeBytes),
    events_cursor: c.eventsCursor !== undefined ? Number(c.eventsCursor) : null,
    recoverable: c.recoverable,
    is_latest: c.isLatest,
  };
}

export function useSessionCheckpoints(sessionId: string | undefined) {
  return useQuery(
    listCheckpoints,
    { sessionId: sessionId ?? "" },
    {
      enabled: sessionId !== undefined,
      refetchInterval: POLL_INTERVAL_MS,
      refetchOnWindowFocus: false,
      select: (resp): CheckpointsResponse => ({
        session_id: resp.sessionId,
        checkpoints: resp.checkpoints.map(protoCheckpointToCheckpointSummary),
      }),
    },
  );
}
