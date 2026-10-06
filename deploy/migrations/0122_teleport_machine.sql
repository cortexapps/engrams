ALTER TABLE sessions
    DROP COLUMN evac_attempts,
    DROP COLUMN teleport_target_host_id,
    DROP COLUMN teleport_target_set_at;
DROP INDEX IF EXISTS idx_sessions_evacuating;

-- A successor restores from the same export without repeating presetup.
ALTER TABLE session_teleports ADD COLUMN live_payload JSONB;

-- A session mid-move under the retired evacuation scanner has no
-- session_teleports row, and from this version nothing else drives
-- `evacuating`. Settle those rows the way a lost host settles (the
-- dead-host orphan step): entomb the bound sandbox, drop the binding, and
-- keep the live disk manifest. Idle when a recoverable memory snapshot OR a
-- live disk manifest exists (the latter resumes through the disk-only cold
-- boot); Dead only with nothing recoverable. Rows with an open teleport
-- belong to the new machine.
INSERT INTO sandbox_tombstones (host_id, sandbox_id, session_id, created_at)
SELECT host_id, sandbox_id, id, now()
  FROM sessions
 WHERE status = 'evacuating'
   AND host_id IS NOT NULL
   AND sandbox_id IS NOT NULL
   AND NOT EXISTS (
         SELECT 1 FROM session_teleports t
          WHERE t.session_id = sessions.id
            AND t.phase NOT IN ('done', 'aborted', 'failed'))
ON CONFLICT DO NOTHING;

UPDATE sessions
   SET status = CASE
                  WHEN live_disk_manifest_id IS NOT NULL
                    OR EXISTS (SELECT 1 FROM snapshots s
                                WHERE s.session_id = sessions.id AND s.recoverable)
                  THEN 'idle' ELSE 'dead'
                END,
       host_id = NULL,
       sandbox_id = NULL,
       updated_at = now()
 WHERE status = 'evacuating'
   AND NOT EXISTS (
         SELECT 1 FROM session_teleports t
          WHERE t.session_id = sessions.id
            AND t.phase NOT IN ('done', 'aborted', 'failed'));
