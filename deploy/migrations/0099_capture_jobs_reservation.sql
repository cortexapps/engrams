-- ADR 0084 known-gap (c): move the ADR 0081 capture placement
-- reservation OFF `enable_jobs` and ONTO `capture_jobs` — the durable,
-- epoch-fenced job row that already owns capture execution.
--
-- #621 (migration 0095) parked the reservation on `enable_jobs`
-- (`capture_host_id` / `capture_waiting_since` / `mem_budget_mib` /
-- `cpu_budget_vcpus`) because that was the only capture-scoped row then.
-- ADR 0084 introduced `capture_jobs`, which is where a capture VM's
-- lifecycle actually lives. Keeping the reservation on `enable_jobs`
-- would split a capture's identity across two rows and force an explicit
-- `clear_capture_reservation` on every terminal transition; anchoring it
-- on `capture_jobs` makes RELEASE IMPLICIT — a terminal `stage` drops
-- the row out of every reserved-SUM via the same
-- `stage NOT IN ('done','failed')` filter the anti-affinity /
-- assignment reads already use.
--
--   * mem_budget_mib / cpu_budget_vcpus — stamped at `insert_capture_job`
--     from `ImageConfig::resolved_memory_mib` / `resolved_vcpus` (the
--     same derivation sessions reserve with). NOT NULL DEFAULT 0; every
--     row inserted from this commit forward supplies real budgets.
--   * host_id becomes NULLABLE: a NULL host_id is a WAITING capture — no
--     host fit yet, NOT dispatchable (heartbeat dispatch keys on
--     host_id), counted as queued demand for the K4 autoscaler, and
--     re-attempted on every scanner tick until it places or the queue
--     timeout expires.
--   * waiting_since — first moment the capture found no fitting host
--     (COALESCE-stamped, restart-proof); the DB-anchored wait deadline
--     the coordinator measures the queue timeout from. NULL once placed.

ALTER TABLE capture_jobs
    ADD COLUMN mem_budget_mib   BIGINT  NOT NULL DEFAULT 0,
    ADD COLUMN cpu_budget_vcpus INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN waiting_since     TIMESTAMPTZ;

-- A NULL host_id == waiting-for-capacity. The `capture_jobs_active_host`
-- partial index (created in 0096) simply won't index the NULL-host rows,
-- which is exactly right: a waiting job is not dispatchable to any host.
ALTER TABLE capture_jobs ALTER COLUMN host_id DROP NOT NULL;

-- Retire the superseded #621 reservation columns from `enable_jobs` —
-- nothing reads them anymore (clean break per CLAUDE.md). Every
-- reserved-SUM reader (session placement, per_host_reserved,
-- fleet_free_mib, queued_demand) now reads `capture_jobs`.
ALTER TABLE enable_jobs
    DROP COLUMN capture_host_id,
    DROP COLUMN capture_waiting_since,
    DROP COLUMN mem_budget_mib,
    DROP COLUMN cpu_budget_vcpus;
