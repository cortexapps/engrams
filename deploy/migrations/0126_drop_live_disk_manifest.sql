-- The session disk lives on the host's thin pool; a session's disk
-- lineage is the latest recoverable snapshot's manifest, nothing else.
DROP INDEX IF EXISTS idx_sessions_live_disk_manifest;
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_live_disk_manifest_both_or_neither,
    DROP COLUMN live_disk_manifest_id,
    DROP COLUMN live_disk_manifest_version,
    DROP COLUMN live_disk_manifest_at;
