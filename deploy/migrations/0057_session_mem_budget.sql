-- ADR 0046: PG-backed memory reservation for session placement.
--
-- `mem_budget_mib` is the session's guest-RAM reservation on its host. It is set
-- at create from the image manifest's `suggested_memory_mib`; Firecracker
-- hard-caps the VM at this, so it is an exact upper bound, not an estimate.
--
-- A session bound to a host (`host_id` set, `status` in the active set) IS a
-- live reservation: the placement transaction sums these per host (plus the
-- enabled-image residency floor) to compute free RAM and rejects rather than
-- overcommit. The reservation releases automatically when the session leaves the
-- host (idle / dead / off-host), so there is no separate reservation table and
-- no stale-reservation reaper — reservation lifecycle = session lifecycle.
--
-- The partial index on (host_id, status) serves that per-host aggregate, run on
-- every create/restore placement under `SELECT … FROM hosts … FOR UPDATE`.
--
-- Additive: existing rows default to 0. Legacy in-flight sessions therefore
-- reserve nothing until they terminate — harmless and transient; every new
-- session sets a real budget.

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS mem_budget_mib BIGINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_sessions_host_status
    ON sessions (host_id, status)
    WHERE host_id IS NOT NULL;
