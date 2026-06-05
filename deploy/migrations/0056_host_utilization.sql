-- Persist observed host utilization (disk / memory / CPU) on the
-- `hosts` row, sampled by the host-agent on every heartbeat.
--
-- Why on the row and not just in-memory: heartbeats land on whichever
-- coord replica the L4 LB picks, so any single pod's in-memory
-- `host_registry` only has a recent sample for the subset of hosts
-- that happened to heartbeat to it. `/api/hosts` therefore reads
-- utilization back from Postgres (same pattern as the capacity_*_mib
-- columns in 0023) so the operator fleet view is consistent across
-- replicas instead of flashing to zero on every other poll.
--
-- This is *observed* utilization (what the host is actually using),
-- distinct from the capacity_*_mib reservation columns the scheduler
-- reasons about. Disk is the one that matters most operationally —
-- the chunk cache + FC memory dumps fill the work_dir mount and a
-- full disk bricks the host silently.
--
-- All columns nullable-free with DEFAULT 0 so the migration is purely
-- additive; a host-agent that predates the probe simply leaves them
-- at 0 (rendered as an empty bar) until it rolls.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_disk_total_mib BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_disk_used_mib  BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_mem_total_mib  BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_mem_used_mib   BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_cpu_pct        REAL    NOT NULL DEFAULT 0;
