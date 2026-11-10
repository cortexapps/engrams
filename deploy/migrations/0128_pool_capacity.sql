-- Placement gates on the session-disk pool's free space, which the host
-- reports every heartbeat. There are no device slots to count.
ALTER TABLE hosts
    DROP COLUMN nbd_slots_total,
    DROP COLUMN nbd_slots_in_use,
    DROP COLUMN nbd_sandboxes,
    ADD COLUMN pool_data_free_mib BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN pool_data_total_mib BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN pool_metadata_free_mib BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN pool_metadata_total_mib BIGINT NOT NULL DEFAULT 0;
ALTER TABLE sessions DROP COLUMN nbd_slot_need;
