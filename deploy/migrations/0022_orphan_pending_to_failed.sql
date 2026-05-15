-- Mark long-orphaned Pending sessions as Failed.
--
-- The create-session handler used to insert with status=pending
-- *before* scheduling. If any step between the insert and the
-- subsequent `set_session_status(Active)` failed (capacity-fit
-- rejection, intermediate I/O error, request abandoned mid-flight),
-- the row stuck in `pending` with `host_id IS NULL` forever — no
-- background process ever advanced it.
--
-- The handler now persists atomically after a successful schedule
-- (Phase 5 of `create_session`), so this class of orphan can't be
-- created anymore. This migration sweeps the historical rows.
--
-- Conservative window (10 minutes): a row that has been pending for
-- longer than that with no host bound is unambiguously stuck —
-- scheduling completes in single-digit seconds in normal operation.
-- Anything outside this window stays as-is.
UPDATE sessions
SET status = 'failed',
    updated_at = NOW()
WHERE status = 'pending'
  AND host_id IS NULL
  AND created_at < NOW() - INTERVAL '10 minutes';
