-- Phase 3d follow-up: dead-host auto-detector polls
--   WHERE status IN ('ready','draining') AND last_heartbeat_at < NOW() - INTERVAL ?
-- every ~10s. Without an index on (status, last_heartbeat_at) the
-- query is a full table scan over `hosts`. Tiny today, but every
-- coordinator replica runs the same query, so the cost grows with
-- (replicas × hosts). Partial-index on the non-terminal statuses
-- since 'dead' rows are filtered out anyway.

CREATE INDEX IF NOT EXISTS idx_hosts_stale_heartbeat
    ON hosts (status, last_heartbeat_at)
    WHERE status IN ('ready', 'draining');
