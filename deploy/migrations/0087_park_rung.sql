-- ADR 0074: parking-ladder rung bookkeeping. park_rung is meaningful
-- only while status IN ('evicting','idle'): 0 = none (running / plain
-- idle / evicted-remote), 1 = nominated (VM untouched), 2 =
-- parked-paused, 3 = parked-local. parked_at stamps the rung entry
-- (rung-2 dwell cap; rung-3 retention accounting).
--
-- NOTE: numbered 0087 — 0086 is held by #592's drop_sessions_queue_prompt
-- (landed with the ADR 0073 create-path hardening). ADD COLUMN with a
-- constant DEFAULT is metadata-only in PG 11+ (no table rewrite).
ALTER TABLE sessions ADD COLUMN park_rung SMALLINT NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN parked_at TIMESTAMPTZ;
