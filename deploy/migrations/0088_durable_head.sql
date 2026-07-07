-- ADR 0077 phase 1: per-session durable head. Row existence ==
-- durability; the head names the newest fully-committed snapshot
-- (blobs uploaded + row inserted in one transaction). Resume selects
-- by this instead of scanning + HEAD-verifying candidates (phase 5).
--
-- Numbered 0088: main high-water at land = 0087 (park_rung).
ALTER TABLE sessions
    ADD COLUMN durable_head_snapshot_id UUID REFERENCES snapshots(id) ON DELETE SET NULL;
CREATE INDEX idx_sessions_durable_head ON sessions (durable_head_snapshot_id)
    WHERE durable_head_snapshot_id IS NOT NULL;
