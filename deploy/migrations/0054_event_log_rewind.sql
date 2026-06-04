-- ADR 0028 A.log: epoch-versioned event log for rung-1 recovery rewind.
--
-- A rung-1 recovery restores a coherent guest at checkpoint time T,
-- but `session_events` was written in real time up to crash time
-- T+Δ. Left alone, the transcript shows messages + tool calls the
-- resumed agent never made (from its perspective). So on a rung-1
-- rewind we TOMBSTONE the events after the checkpoint's
-- `events_cursor` (kept for audit, not deleted) and reset the live
-- head; subsequent events carry an incremented recovery epoch so the
-- timeline is unambiguous.

-- The session's current recovery epoch. Bumped on each rung-1 rewind;
-- new events stamp this value so consumers can tell pre/post-rewind
-- segments apart.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS recovery_epoch INTEGER NOT NULL DEFAULT 0;

-- Per-event epoch (the session's epoch when the event was appended)
-- and the tombstone marker. `rewound_at IS NOT NULL` = rolled back by
-- a recovery; the row stays for audit + a collapsed/greyed render,
-- but it's not part of the live transcript head.
ALTER TABLE session_events
    ADD COLUMN IF NOT EXISTS recovery_epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE session_events
    ADD COLUMN IF NOT EXISTS rewound_at TIMESTAMPTZ;

-- The rewind tombstones a tail span by idx; the SSE replay reads the
-- whole log ordered by idx and renders rewound rows collapsed, so no
-- new index is needed (the existing (session_id, idx) PK covers it).
