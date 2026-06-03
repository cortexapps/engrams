-- VZ parity: the base snapshot's *memory* manifest is no longer required.
--
-- Migration 0043 made base_snapshot_memory_manifest_{id,version} NOT NULL on
-- the premise that "a base snapshot is always a chunked FC memory snapshot."
-- That holds for Firecracker but not for the macOS Virtualization.framework
-- (VZ) backend: Apple's restoreMachineStateFromURL is broken for arm64 Linux
-- guests (ADR 0003), so VZ clone-snapshots the disk and *cold-boots* on
-- restore. There is no memory image to chunk, hence no memory manifest.
--
-- Disk residency still applies to VZ (the rootfs clone is chunked, migration
-- 0042's base_snapshot_disk_manifest stays NOT NULL). Only memory residency is
-- meaningless for a cold-boot backend, so we relax just the memory columns to
-- nullable. FC continues to populate them; VZ leaves them NULL and the
-- host-boot prefetch / residency advertisement skip the memory tier.

ALTER TABLE enabled_images
    ALTER COLUMN base_snapshot_memory_manifest_id      DROP NOT NULL,
    ALTER COLUMN base_snapshot_memory_manifest_version DROP NOT NULL;
