-- ADR 0020 P1: every enabled image must have a base snapshot.
--
-- Session create restores from a per-image base snapshot unconditionally —
-- there is no cold-boot path (the only cold boot in the system is the
-- capture itself, run once per image at enable). So "an enabled image with
-- no base snapshot" is an unservable state. Rather than enforce that only in
-- application code, make it a schema invariant: a NOT NULL FK from
-- enabled_images to the snapshots row produced at enable time.
--
-- enable_image / refresh_enabled_image capture the base snapshot on a host
-- and record its snapshots row BEFORE upserting the enabled_images row, so
-- the FK is always satisfiable; a capture failure aborts the enable and no
-- enabled_images row is written.
--
-- Every enabled_images row that exists before this migration predates
-- base-snapshot capture and so cannot satisfy the new NOT NULL column.
-- Clear them out (operators re-enable each image, which now captures a base
-- snapshot transactionally) and only then add the column. Sessions already
-- running are unaffected — they don't re-check enablement; only new session
-- creates require a re-enable.
DELETE FROM enabled_images;

ALTER TABLE enabled_images
    ADD COLUMN base_snapshot_id UUID NOT NULL REFERENCES snapshots(id);
