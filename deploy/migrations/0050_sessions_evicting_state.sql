-- ADR 0034: extend the `sessions_status_check` CHECK constraint to
-- allow the new `evicting` variant, add the per-session retry counter
-- for the coord-side eviction scanner, and index `session_events` for
-- the PG idle-detection backstop.
--
-- `evicting` is the durable eviction-intent marker: the candidates
-- handler (or the detection backstop) transitions Active → Evicting
-- and returns immediately; the eviction scanner sweeps this state and
-- drives the snapshot pipeline to its terminal Evicting → Idle. The
-- row survives coord restarts — the next pod's scanner picks it up on
-- its first tick.
--
-- Drop + re-add in place (one transactional swap) matches the
-- migration 0031/0037 pattern.

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
            'evicting',
            'completed',
            'failed',
            'dead'
        )
    );

-- Per-session eviction retry counter (mirrors `evac_attempts`, 0037).
-- Deliberately a SEPARATE column: a session can traverse both flows
-- across its lifetime and the two scanners' budgets must not
-- cross-contaminate.
--
-- The eviction scanner bumps this before each pipeline attempt. After
-- 20 bumps (~3 min at the 10s cadence) it gives up and falls back to
-- `host_lost` (the honest "coord can't reconcile this runtime" state;
-- see ADR 0034 for why active/idle/dead are each wrong).
--
-- Reset semantics: `transition_session(target=evicting)` clears the
-- counter so re-entry (a later idle period after a successful resume)
-- starts the budget fresh.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS evict_attempts INTEGER NOT NULL DEFAULT 0;

-- Eviction-scanner sweep query: `WHERE status = 'evicting'`. Small
-- working set at any moment (sessions mid-eviction); partial index
-- keeps the per-tick scan flat regardless of total row count.
CREATE INDEX IF NOT EXISTS idx_sessions_evicting
    ON sessions (id)
    WHERE status = 'evicting';

-- Detection backstop (ADR 0034 L3): the coord-side scanner asks, per
-- Active session, "when was the newest session_events row?" — i.e.
-- MAX(created_at) per session_id. No index covered created_at before
-- (the PK is (session_id, idx)); this composite makes the per-session
-- MAX an index-only descent.
CREATE INDEX IF NOT EXISTS idx_session_events_session_created
    ON session_events (session_id, created_at DESC);
