-- ADR 0035 amendment D2: per-running-sandbox aux bundle attachments, reported by
-- the host heartbeat. `bundle_pin_set` unions these so a sandbox that
-- exists but has not snapshotted yet pins its generations against the
-- host sweep and the bundle GC (the 2026-08-10 chain_poisoned firing
-- rode exactly this gap). Shape: [{"sandbox_id": "...", "bundles":
-- [{"drive_id": "...", "sha256": "..."}]}].
ALTER TABLE hosts
    ADD COLUMN sandbox_bundles JSONB NOT NULL DEFAULT '[]'::jsonb;
