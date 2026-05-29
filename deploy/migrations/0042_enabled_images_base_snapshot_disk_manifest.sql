-- ADR 0021 P2: persist the base snapshot's *disk* manifest on the
-- enabled_images row so the heartbeat advertisement can carry it to hosts,
-- which warm those chunks on NVMe (residency) before sessions restore.
--
-- The substrate cost measured in prod (Cloud Trace 4bdd4903, session
-- 49272389) is the resuming guest paging in its rootfs — 16 MiB *disk*
-- chunks, served on-demand and serially from GCS over NBD — because the disk
-- daemon has no prefetch and the host-boot image prefetch only covered the
-- *image* disk manifest, not the base snapshot's (which carries the runtime
-- files written at template-boot: Bun / node_modules / claude). Denormalizing
-- the base snapshot's disk ManifestRef here lets the host warm exactly those
-- chunks without a per-heartbeat snapshot lookup.
--
-- The ref is known at enable time in
-- `crates/engram-coordinator/src/api/enabled_images.rs::capture_and_record_base_snapshot`
-- (the SnapshotMetadata returned by build_base_snapshot). It is the disk
-- manifest of the snapshot already referenced by the NOT NULL
-- base_snapshot_id column.
--
-- Columns are NULLable (mirroring disk_manifest): a harness-less / dev-VM
-- base snapshot with no chunked-disk artifact legitimately has none, and rows
-- enabled before this migration keep NULL until re-enabled. The host
-- tolerates NULL by skipping the residency prefetch for that image. The
-- both-or-neither CHECK keeps the (id, version) pair coherent.

ALTER TABLE enabled_images
    ADD COLUMN IF NOT EXISTS base_snapshot_disk_manifest_id      UUID,
    ADD COLUMN IF NOT EXISTS base_snapshot_disk_manifest_version BIGINT;

ALTER TABLE enabled_images
    DROP CONSTRAINT IF EXISTS enabled_images_base_snapshot_disk_manifest_both_or_neither;
ALTER TABLE enabled_images
    ADD CONSTRAINT enabled_images_base_snapshot_disk_manifest_both_or_neither CHECK (
        (base_snapshot_disk_manifest_id IS NULL) = (base_snapshot_disk_manifest_version IS NULL)
    );
