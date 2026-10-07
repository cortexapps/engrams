-- Snapshot swap uses the same reference types as the root disk.
ALTER TABLE snapshots
    ADD COLUMN swap_manifest_id UUID,
    ADD COLUMN swap_manifest_version BIGINT,
    ADD CONSTRAINT snapshots_swap_manifest_both_or_neither
        CHECK ((swap_manifest_id IS NULL) = (swap_manifest_version IS NULL));
