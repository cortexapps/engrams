-- ADR 0016 §A.1.5c: cross-replica idle-eviction guard via PG
-- leasing row. Replaces A.1.5b's per-pod `DashMap<SessionId, ()>`
-- with a row keyed by `session_id` so a second coord pod entering
-- `evict_idle_session` for an already-in-flight session sees the
-- conflict and returns early. One-at-a-time-per-session
-- enforcement is the design contract.
--
-- Why a row, not `pg_advisory_lock`:
--   1. Connection-scoped. `pg_advisory_lock` is held by the
--      backend session; sqlx connection pooling means the lock and
--      its release must run on the SAME pooled connection, which
--      is fragile to reason about and breaks under reuse.
--   2. Operator-visible. `SELECT * FROM eviction_inflight` answers
--      "which sessions are mid-eviction on which pod?" in one
--      query. `pg_locks` is noisier and lockd-id encoded.
--   3. Reapable. A background task can sweep stale entries with a
--      `tracing::warn!`; no equivalent hook for advisory locks.
--
-- The RAII guard's `Drop` deletes the row best-effort. A stale
-- entry (e.g. coord pod OOM'd mid-pipeline) is reaped by a coord-
-- side background task at the same 180s threshold A.1.5a uses
-- host-side.
CREATE TABLE eviction_inflight (
    session_id  UUID PRIMARY KEY,
    -- Coord pod hostname / unique identifier. Carried for
    -- operator diagnostics ("which pod's pipeline is wedged?") —
    -- not part of any uniqueness constraint.
    locked_by   TEXT NOT NULL,
    -- For the stale-lease sweeper to compare against `now()`.
    -- TIMESTAMPTZ to match every other timestamp in the schema.
    locked_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The sandbox the pipeline is acting on. Diagnostic only —
    -- if a session resumes and re-idles between two pods racing,
    -- the second pod's `evict_idle_session` may target a
    -- different sandbox_id, and we want the operator query to
    -- show that mismatch.
    sandbox_id  UUID NOT NULL
);

-- Sweeper query lookup: WHERE locked_at < now() - interval '180 seconds'.
-- Partial-index is overkill for the expected row count (handfuls at any
-- moment); the BRIN-ish range walk over a few rows is fine.
CREATE INDEX IF NOT EXISTS idx_eviction_inflight_locked_at
    ON eviction_inflight (locked_at);
