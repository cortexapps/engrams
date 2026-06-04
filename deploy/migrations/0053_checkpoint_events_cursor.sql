-- ADR 0028 Fix A / A.log: periodic coherent checkpoints.
--
-- A checkpoint is coherent as a TRIPLE — (memory, disk, event-log
-- cursor). The memory + disk manifests already live on the snapshots
-- row; this adds the third leg: the `session_events.idx`
-- high-water-mark at the checkpoint's pause instant. A rung-1
-- recovery rewinds the transcript's live head to this cursor
-- (tombstoning, not deleting — A.log); a fork (ADR 0022 horizon)
-- cuts the child's transcript here.
--
-- NULL semantics: pre-0053 rows, template snapshots (session_id IS
-- NULL), and captures whose recording coord couldn't resolve the
-- cursor. Rung-1 treats NULL as "no rewind information — surface the
-- recovery boundary without tombstoning".
ALTER TABLE snapshots
    ADD COLUMN IF NOT EXISTS events_cursor BIGINT;

-- The checkpoint reconciler + retention sweeper walk a session's
-- checkpoint chain newest-first ("latest per session", "older than
-- the retention window"). The existing snapshots indexes are keyed
-- for GC manifest walks, not per-session recency.
CREATE INDEX IF NOT EXISTS idx_snapshots_session_created
    ON snapshots (session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
