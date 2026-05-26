-- ADR 0018 commit 12b: extend the `sessions_status_check` CHECK
-- constraint to allow the new `evacuating` variant, and add the
-- per-session retry counter for the `evac_resumer` background
-- scanner.
--
-- `evacuating` was added to the Rust SessionState enum but never to
-- the DB CHECK constraint, so every transition_session(Evacuating)
-- failed with `new row for relation "sessions" violates check
-- constraint "sessions_status_check"`. Caught on dev-vm during
-- e2e validation of the async evac path.
--
-- Drop + re-add in place (one transactional swap) matches the
-- migration 0031 pattern that introduced created/guest_ready/host_lost.

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_status_check;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_status_check CHECK (
        status IN (
            'pending',
            'created',
            'guest_ready',
            'active',
            'idle',
            'host_lost',
            'evacuating',
            'completed',
            'failed',
            'dead'
        )
    );

-- Per-session retry counter (described above).
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
