-- Persistent per-session event log. Every SessionEvent the coordinator
-- emits — lifecycle transitions, exec start/end, stdout/stderr chunks,
-- snapshot/evict/resume — lands here. Subscribers reconnecting via
-- `?since=N` (or EventSource's auto-`Last-Event-ID` header) replay
-- from this table before tailing the live in-memory bus.
--
-- Per-session idx allocation is atomic via the `next_event_idx`
-- counter on `sessions`: a single UPDATE ... RETURNING in the same
-- transaction as the INSERT guarantees monotonic, gap-free ordering
-- under concurrent publishers.

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS next_event_idx BIGINT NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS session_events (
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    idx BIGINT NOT NULL,
    -- Discriminant matches `SessionEvent::kind()` in `engram-coordinator`.
    -- Values: status_changed | exec_started | exec_completed | stdout
    -- | stderr | snapshot_taken | evicted | resumed.
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_id, idx)
);

-- Replay queries hit `WHERE session_id = $1 AND idx > $2 ORDER BY idx`.
-- The PK already covers (session_id, idx) so no extra index needed.
