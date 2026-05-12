-- ADR 0007 / Phase 7: retire the cold-tier flush pipeline.
--
-- ADR 0005 added a "hot tier (`local_path`) + cold tier (envelope-
-- encrypted sealed blob)" durability scheme for snapshots. ADR 0007
-- replaced that with chunked-immutable storage: every snapshot
-- references content-addressed chunks in `BlobStorage` via
-- `disk_manifest_*` (migration 0018) and `memory_manifest_*`
-- (migration 0019). Both Rust code paths and the Postgres schema
-- carried the legacy columns alongside the new fields through
-- Phases 5-6 so each phase could ship + revert independently.
--
-- This migration deletes the legacy columns. After 0020:
--   - `snapshots` rows reference durability ONLY through the chunked
--     manifests; the per-host directory path is reconstructed from
--     `(cfg.local_path, session_id, snapshot_id)` on demand.
--   - `SessionStatus::ColdEvicted` is gone; sessions live in
--     `idle` or `dead` post-Phase-7.
--   - The `cold_evicted_at` column on `sessions` is dropped.
--   - The `'cold_evicted'` value is removed from the
--     `sessions_status_check` CHECK constraint.
--
-- Production has zero deployments today (project pre-release), so
-- the migration drops columns outright with no backfill. Future
-- deployments that need to recover legacy rows can roll their own
-- restore from a pg_dump backup.

-- 1. Drop the cold-tier sealed-blob-ref envelope-encryption quartet
--    + presence flag + replication timestamp + local path.
ALTER TABLE snapshots
    DROP COLUMN IF EXISTS wrapped_dek,
    DROP COLUMN IF EXISTS nonce,
    DROP COLUMN IF EXISTS ciphertext,
    DROP COLUMN IF EXISTS key_id,
    DROP COLUMN IF EXISTS blob_present,
    DROP COLUMN IF EXISTS replicated_at,
    DROP COLUMN IF EXISTS local_path;

-- 2. Drop the cold-evict timestamp on `sessions`. The status enum
--    no longer carries `cold_evicted` so the column is dead weight.
ALTER TABLE sessions
    DROP COLUMN IF EXISTS cold_evicted_at;

-- 3. Update the sessions.status CHECK constraint to drop
--    `cold_evicted`. The constraint name follows 0009's convention;
--    rebuild it in place so production deploys see one transactional
--    swap rather than a constraint-less window.
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_status_check;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_status_check CHECK (
        status IN ('pending', 'active', 'idle', 'completed', 'failed', 'dead')
    );

-- 4. Drop the indexes that supported cold-tier queries. The chunked
--    GC uses `idx_snapshots_disk_manifest` (migration 0018) +
--    `idx_snapshots_memory_manifest` (0019); no need for a
--    `blob_present` index now.
DROP INDEX IF EXISTS idx_snapshots_blob_present;
DROP INDEX IF EXISTS idx_snapshots_residency;
