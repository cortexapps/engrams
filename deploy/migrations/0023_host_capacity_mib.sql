-- Add MiB-precision capacity columns to `hosts` so heartbeats can
-- persist real numbers, not the GB-rounded values the row used to
-- carry (those were always 0 in practice — see HostCapacity).
--
-- Why: with the coord scaled to >1 replica, each host's heartbeat
-- WS pins to ONE pod. The other pod's in-memory `host_registry`
-- has no entry for that host, so when its turn comes to serve
-- `/api/hosts` it falls back to the Postgres row. Without these
-- columns the fallback yielded zero capacity and the SPA's host
-- panel flashed between the real value (pod that owns the WS) and
-- zero (pod that doesn't) on each poll.
--
-- New columns are nullable + default 0 so the migration is a pure
-- additive change. The heartbeat handler is updated in the same
-- commit to populate them on every tick.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS capacity_total_mib BIGINT NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS capacity_used_mib  BIGINT NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS running_sandboxes_count INTEGER NOT NULL DEFAULT 0;
