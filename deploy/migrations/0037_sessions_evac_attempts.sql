-- ADR 0018 commit 12b: per-session retry counter for the
-- `evac_resumer` background scanner.
--
-- A session enters `evacuating` either via operator drain (admin
-- /drain or /sessions/:id/evacuate) or the dead-host detector. The
-- scanner picks it up, picks a peer host, runs the resume pipeline,
-- and on success transitions Evacuating → Created → Active. On
-- transient failure (peer host transient error, no capacity, image
-- not enabled there yet) it bumps `evac_attempts`. After 20 bumps
-- (~3 min at the 10s scanner cadence) the scanner gives up and
-- falls back to `Idle` so the user can /resume manually.
--
-- Reset semantics: `transition_session(target=evacuating)` clears
-- the counter so re-entry from another drain starts the budget
-- fresh — important when the same session gets drained, succeeds
-- on a peer, then later gets drained again from the new peer.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS evac_attempts INTEGER NOT NULL DEFAULT 0;

-- Scanner sweep query: `WHERE status = 'evacuating'`. Sessions in
-- Evacuating are expected to be a small handful at any moment
-- (drain-in-progress + recent dead-host orphans). Partial index
-- keeps the cost of the per-tick scan flat regardless of total
-- session row count.
CREATE INDEX IF NOT EXISTS idx_sessions_evacuating
    ON sessions (id)
    WHERE status = 'evacuating';
