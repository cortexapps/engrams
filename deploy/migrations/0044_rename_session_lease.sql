-- Rename the `eviction_inflight` lease table to `session_lease`.
--
-- ADR 0016 §A.1.5c introduced this table as an idle-eviction-only
-- guard. It has since become a general per-session operation lease:
-- the resume path (`api::snapshot::resume_session`) now takes the
-- same lease so a resume can't race an eviction (or another resume)
-- and orphan a sandbox. The "eviction" name no longer fits; rename
-- the table, its locked_at index, and the primary-key constraint to
-- match the Rust-side `session_lease` / `SessionLeaseGuard` naming.
--
-- Also drop NOT NULL on `sandbox_id`: it is a diagnostic-only column
-- ("which sandbox is this op acting on?"). An eviction has a real
-- sandbox (Some); a resume has none yet — it's about to create one —
-- so resume now stores NULL rather than a nil-UUID sentinel.
--
-- Deploy note: this is NOT backward-compatible. An old coord pod
-- still in the rolling window queries `eviction_inflight` and will
-- error on its lease INSERT/DELETE until it is replaced — idle
-- eviction retries on the next host tick and resume returns 5xx for
-- the ~1-2 min overlap. Acceptable here (pre-GA, no live sessions);
-- a zero-downtime variant would add a compatibility view first.
ALTER TABLE eviction_inflight RENAME TO session_lease;

ALTER TABLE session_lease ALTER COLUMN sandbox_id DROP NOT NULL;

ALTER INDEX idx_eviction_inflight_locked_at RENAME TO idx_session_lease_locked_at;

ALTER TABLE session_lease RENAME CONSTRAINT eviction_inflight_pkey TO session_lease_pkey;
