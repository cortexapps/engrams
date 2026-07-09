-- ADR 0081: capture placement rides the session scheduler.
--
-- A base-snapshot capture boots a VM sized like a session of the same
-- image, but held no placement reservation — `pick_capture_host` was
-- first-fit with no RAM/CPU gate, so a capture could land next to a
-- same-sized session on one node and OOM it (prod 2026-07-08, session
-- 8a80c3fb). Captures now reserve through the same 2D fit as sessions:
--
--   * mem_budget_mib / cpu_budget_vcpus — stamped at job creation from
--     the image config (`resolved_memory_mib` / `resolved_vcpus`, the
--     same derivation sessions reserve with);
--   * capture_host_id — the atomically-reserved capture host, set by
--     `reserve_capture_host`'s FOR UPDATE pick for the duration of the
--     capture step (NULL outside it). Every reserved-SUM reader
--     (session placement, per_host_reserved, fleet_free_mib) UNIONs
--     capturing jobs in via this column;
--   * capture_waiting_since — first moment the capture found no host
--     that fits (NULL once placed). Waiting captures count as queued
--     demand for the K4 autoscaler and fail legibly past the queue
--     timeout instead of burning the attempts budget.

ALTER TABLE enable_jobs
    ADD COLUMN mem_budget_mib BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN cpu_budget_vcpus INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN capture_host_id UUID,
    ADD COLUMN capture_waiting_since TIMESTAMPTZ;
