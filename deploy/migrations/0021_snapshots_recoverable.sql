-- ADR 0009: snapshot recoverability flag.
--
-- The ADR's reconcile pass intersects the host's heartbeat-reported
-- `running_sandboxes` against expected-active `sessions` rows.
-- When a session's sandbox is missing from N consecutive heartbeats,
-- the reconcile pass transitions the session:
--
--   - to `Idle` if its latest snapshot is durably recoverable
--     (i.e. the canonical chunked manifests are present in
--     BlobStorage and reachable on resume), OR
--   - to `Dead` otherwise.
--
-- `recoverable` is set TRUE by the snapshot completion path after
-- BlobStorage HEAD-verifying the manifest's chunks landed durably;
-- it's set FALSE by the chunk-store GC when it reaps a referenced
-- manifest.
--
-- Default FALSE for safety: a pre-migration snapshot row with no
-- `recoverable` evidence flips its session to `Dead` rather than
-- promising an Idle/resume path we can't deliver. Phase 2 of ADR
-- 0009's rollout populates the column for new snapshots; the old
-- ones stay FALSE until they're re-taken or reaped.

ALTER TABLE snapshots
    ADD COLUMN IF NOT EXISTS recoverable BOOLEAN NOT NULL DEFAULT false;

-- Partial index for the reconcile pass's lookup ("does this
-- session's latest snapshot row have recoverable=true?"). Most rows
-- are `recoverable=true` in steady state, so the partial index
-- saves space + write cost during GC sweeps that flip the flag back
-- to false.
CREATE INDEX IF NOT EXISTS idx_snapshots_recoverable_by_session
    ON snapshots (session_id, created_at DESC)
    WHERE recoverable = true;
