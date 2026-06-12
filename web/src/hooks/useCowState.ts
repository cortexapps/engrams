import { useQuery } from "@connectrpc/connect-query";
import { getCowState } from "../gen/engram/app/v1/session-SessionService_connectquery";
import type { CowStateView as ProtoCowStateView } from "../gen/engram/app/v1/session_pb";
import type { CowStateView, SessionCowStateResponse } from "../lib/types";

// ADR 0016 Phase A: per-session COW diagnostic data.
//
// The endpoint is cached at coord with a 1s TTL, so polling at 2s
// keeps the freshness budget comfortable while avoiding a per-poll
// host RPC. The web app's display doesn't move on sub-second
// timescales — dirty-bytes accumulation is human-paced — so 2s is
// the sweet spot between "feels live" and "doesn't hammer the host".
//
// ADR 0029: the host-wide COW view moved to the Storage surface, which
// reads the coordinator's aggregated storage-summary endpoint rather
// than fanning out per-host cow-state from the browser.
//
// ADR 0039 Task 24: migrated to connect-query via the gated passthrough
// (GetCowState). Returns the legacy SessionCowStateResponse shape so
// CowState.tsx is unchanged. bigint fields converted with Number().
const POLL_INTERVAL_MS = 2_000;

/** Map proto CowStateView (camelCase, bigint) to legacy snake_case shape.
 * dirtyBytes / diskManifestVersion / memoryManifestVersion are bigint in
 * proto — convert with Number() (never exceed MAX_SAFE_INTEGER in practice). */
function protoCowStateViewToCowStateView(v: ProtoCowStateView): CowStateView {
  return {
    sandbox_id: v.sandboxId,
    session_id: v.sessionId ?? null,
    disk_manifest_id: v.diskManifestId,
    disk_manifest_version: Number(v.diskManifestVersion),
    dirty_chunks: v.dirtyChunks,
    dirty_bytes: Number(v.dirtyBytes),
    last_flush_at: v.lastFlushAt ?? null,
    base_chunks: v.baseChunks,
    base_chunks_local: v.baseChunksLocal,
    memory_manifest_id: v.memoryManifestId ?? null,
    memory_manifest_version:
      v.memoryManifestVersion !== undefined ? Number(v.memoryManifestVersion) : null,
    last_snapshot_at: v.lastSnapshotAt ?? null,
  };
}

export function useSessionCowState(sessionId: string | undefined) {
  return useQuery(
    getCowState,
    { sessionId: sessionId ?? "" },
    {
      enabled: sessionId !== undefined,
      refetchInterval: POLL_INTERVAL_MS,
      refetchOnWindowFocus: false,
      select: (resp): SessionCowStateResponse => ({
        session_id: resp.sessionId,
        state: resp.state ? protoCowStateViewToCowStateView(resp.state) : null,
      }),
    },
  );
}
