-- ADR 0047: stateless coordinator — the heartbeat-mirrored scheduling state
-- (image readiness, snapshot locality, bundle stamp) moves into the hosts row
-- so any coordinator replica schedules from the same authority, plus the
-- coordinator-owned cordon bit (heartbeats never write it; only the admin
-- cordon/uncordon endpoints and the ADR 0048 wave driver do).
--
-- total_vcpus is ADR 0048 CPU-packing groundwork: the host's core count as
-- reported by its heartbeat. 0 = not yet reported (pre-roll host-agent).
ALTER TABLE hosts
  ADD COLUMN IF NOT EXISTS ready_images    JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN IF NOT EXISTS local_snapshots JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN IF NOT EXISTS current_bundles JSONB   NOT NULL DEFAULT '[]'::jsonb,
  ADD COLUMN IF NOT EXISTS cordoned        BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN IF NOT EXISTS total_vcpus     INTEGER NOT NULL DEFAULT 0;
