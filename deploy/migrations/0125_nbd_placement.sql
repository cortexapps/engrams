-- Slot counters and the bindings already represented by the sample.
ALTER TABLE hosts
    ADD COLUMN nbd_slots_total BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN nbd_slots_in_use BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN nbd_sandboxes JSONB NOT NULL DEFAULT '{}';
-- One root slot, plus one when the session has swap. New creates stamp
-- the exact need. Existing rows start at one and are raised from the
-- image config they were created under, or from a snapshot that carries
-- a swap manifest. A live sandbox is counted by its host's sample, so a
-- row that keeps one here can never under-reserve a running guest.
ALTER TABLE sessions ADD COLUMN nbd_slot_need BIGINT NOT NULL DEFAULT 1
    CHECK (nbd_slot_need BETWEEN 1 AND 2);
UPDATE sessions s
   SET nbd_slot_need = 2
  FROM enabled_images e
 WHERE e.image_uri = s.image_uri
   AND COALESCE((e.image_config->'resources'->>'suggested_swap_mib')::bigint, 0) > 0;
UPDATE sessions s
   SET nbd_slot_need = 2
 WHERE EXISTS (
     SELECT 1 FROM snapshots sn
      WHERE sn.session_id = s.id AND sn.swap_manifest_id IS NOT NULL
 );
