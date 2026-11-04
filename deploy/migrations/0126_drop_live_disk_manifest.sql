-- The session disk lives on the host's thin pool; a session's disk
-- lineage is the latest recoverable snapshot's manifest, nothing else.
-- A resting row whose only recoverable state was the live manifest has
-- nothing to resume from once the column goes: settle it Dead now
-- rather than let the next resume discover that.
UPDATE sessions
   SET status = 'dead',
       updated_at = now()
 WHERE status IN ('idle', 'host_lost')
   AND live_disk_manifest_id IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM snapshots s
                    WHERE s.session_id = sessions.id AND s.recoverable);
DROP INDEX IF EXISTS idx_sessions_live_disk_manifest;
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_live_disk_manifest_both_or_neither,
    DROP COLUMN live_disk_manifest_id,
    DROP COLUMN live_disk_manifest_version,
    DROP COLUMN live_disk_manifest_at;
