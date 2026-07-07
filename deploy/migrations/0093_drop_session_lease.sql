-- ADR 0079 (issue #543): the durable per-session op log replaced the
-- wall-clock `session_lease` row as the lifecycle mutual exclusion —
-- the `session_ops_one_running` partial unique index (migration 0092)
-- is the serializer, the executor's fence-then-resume reclaim sweep is
-- the successor of the 180s lease reaper. Nothing reads or writes this
-- table anymore.
DROP TABLE IF EXISTS session_lease;
