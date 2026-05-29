-- ADR 0021 P2 (memory residency): persist the base snapshot's *memory*
-- manifest on the enabled_images row — the symmetric companion to migration
-- 0042's base_snapshot_disk_manifest. The heartbeat advertisement carries it to
-- hosts, which warm the memory snapshot's chunks on NVMe at host-boot so a
-- freshly-rolled host's FIRST session restores warm instead of paying a cold
-- per-restore prefetch.
--
-- Why: prod traces after the verify-on-populate fix (c55e1035 warm vs d4cb3728
-- cold) show the disk path is now resident (migration 0042) and cheap, but the
-- base snapshot's memory chunks are still fetched cold from GCS on the first
-- restore per host — ~2.84 s on a freshly-rolled host (fc.restore_in_jail
-- starts at 2857 ms cold vs 156 ms warm). Advertising the memory manifest lets
-- the host-boot prefetch warm it via the exact mechanism already used for disk,
-- and folds memory into the same readiness gate (an image isn't "ready" on a
-- host until BOTH its disk and memory working sets are resident).
--
-- Unlike 0042, no clean break is needed. The ref already exists on the
-- snapshots row referenced by the NOT NULL base_snapshot_id column, so we
-- BACKFILL existing enabled_images from it and reach NOT NULL without a delete
-- or operator re-enable. A base snapshot is always a chunked FC memory snapshot
-- (memory_manifest_id non-NULL); if some row's snapshot lacks one, the SET NOT
-- NULL below fails loudly — correct, since residency requires it.

ALTER TABLE enabled_images
    ADD COLUMN base_snapshot_memory_manifest_id      UUID,
    ADD COLUMN base_snapshot_memory_manifest_version BIGINT;

-- Backfill from the snapshot already pinned by base_snapshot_id.
UPDATE enabled_images e
SET base_snapshot_memory_manifest_id      = s.memory_manifest_id,
    base_snapshot_memory_manifest_version = s.memory_manifest_version
FROM snapshots s
WHERE s.id = e.base_snapshot_id;

ALTER TABLE enabled_images
    ALTER COLUMN base_snapshot_memory_manifest_id      SET NOT NULL,
    ALTER COLUMN base_snapshot_memory_manifest_version SET NOT NULL;
