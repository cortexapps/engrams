-- Track A (ADR 0034/0039 addendum): a truthful per-event liveness signal.
--
-- `last_active_at` only moves on session *state transitions*
-- (`transition_session`), never on event appends. During the `bf3dbbcb`
-- wedge it froze at the resume instant while `session_events` kept landing
-- for another 6m45s — so operators watching `last_active_at` saw a dead
-- session that was actually still emitting, and a desync that keeps
-- emitting looks "active" to the state machine. `last_event_at` is bumped
-- on every `append_session_event`, giving an honest "is this session
-- actually producing output" signal independent of the state machine, and
-- the substrate the run-aware desync watchdog keys off.
--
-- Nullable + no backfill: populated going forward; the watchdog and the
-- existing idle backstop both COALESCE to `created_at` for pre-migration
-- rows with no events.
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS last_event_at timestamptz;
