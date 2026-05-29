-- ADR 0021 P2: persist the base snapshot's *disk* manifest on the
-- enabled_images row so the heartbeat advertisement can carry it to hosts,
-- which warm those chunks on NVMe (residency) before sessions restore.
--
-- The substrate cost measured in prod (Cloud Trace 4bdd4903, session
-- 49272389) is the resuming guest paging in its rootfs — 16 MiB *disk*
-- chunks, served on-demand and serially from GCS over NBD — because the disk
-- daemon has no prefetch and the host-boot image prefetch only covered the
-- *image* disk manifest, not the base snapshot's (which carries the runtime
-- files written at template-boot: Bun / node_modules / claude). The ref is the
-- disk manifest of the snapshot already referenced by the NOT NULL
-- base_snapshot_id column; it is known at enable time in
-- `capture_and_record_base_snapshot` (the SnapshotMetadata from
-- build_base_snapshot).
--
-- Clean break (ADR 0021's "no backwards compatibility" stance): residency
-- requires every enabled image to carry its base snapshot's disk manifest, so
-- the columns are NOT NULL — no nullable-with-fallback, no compat shim, no
-- tolerant decode.

-- (1) Clear the sessions the cutover would otherwise strand. The
-- enabled_images clear below orphans every session's image reference (sessions
-- key images by URI, not an FK), and a deploy also rolls the FC hosts — so the
-- dead-host detector would mark non-terminal (idle / active / …) sessions
-- Evacuating and the evac-resumer would thrash (evac_attempts) resuming them
-- against now-missing image rows: the same failure ADR 0021 P1.8 fixed for the
-- disable path, re-triggered here by a physical delete. Per the ADR's cutover
-- stance ("in-flight sessions are re-created"), clear the non-terminal
-- sessions; terminal ones (dead/completed/failed) are inert history and stay.
-- Every FK into sessions is ON DELETE CASCADE, so dependents (session_events,
-- session_secrets, session snapshots) go with them; base/template snapshots
-- have session_id = NULL and survive (orphaned by the enabled_images clear,
-- reaped by chunk-GC).
DELETE FROM sessions WHERE status NOT IN ('dead', 'completed', 'failed');

-- (2) Clear enabled_images. Existing rows predate the column and can't satisfy
-- NOT NULL; operators re-enable each image, which captures + stamps the base
-- snapshot afresh — exactly as migration 0038 did for base_snapshot_id.
DELETE FROM enabled_images;

ALTER TABLE enabled_images
    ADD COLUMN base_snapshot_disk_manifest_id      UUID   NOT NULL,
    ADD COLUMN base_snapshot_disk_manifest_version BIGINT NOT NULL;
