-- ADR 0007 / Phase 6: chunked-immutable storage durability.
-- Snapshots now reference a content-addressed chunk manifest in
-- BlobStorage instead of (or alongside) the cold-tier sealed blob
-- of ADR 0005. The migration is **additive** in this slice:
-- `disk_manifest_id` + `disk_manifest_version` are added; the
-- cold-tier columns (`local_path`, `blob_present`, the
-- envelope-encryption quartet) stay until Phase 7 deletion lands.
--
-- Why additive: deleting the cold-tier columns now would require
-- simultaneously retiring the MetadataStore methods that read
-- them + the host-agent flush.rs + the disk-pressure detector.
-- All of that is Phase 7. Splitting the schema change from the
-- code retirement keeps each migration commit small enough to
-- review carefully (and easy to revert if production surfaces an
-- issue with persistence shape).
--
-- The `idx_snapshots_disk_manifest` index supports the GC sweep
-- (Tier 4 #4): enumerate manifest_ids that are reachable from
-- any live snapshot row, then sweep chunks not in that set.
-- Pre-Phase-6 the GC would have had to scan every snapshot row;
-- with the index it's a covered scan against a small key set.
--
-- Both columns are nullable because legacy snapshots written
-- before this migration don't have a manifest ref. Phase 7
-- backfills (or just GCs them — ADR 0007 doesn't carry the
-- legacy tar.zst path forward).

ALTER TABLE snapshots
    ADD COLUMN IF NOT EXISTS disk_manifest_id      UUID,
    ADD COLUMN IF NOT EXISTS disk_manifest_version BIGINT;

-- Either both are set (rows produced by the chunked-snapshot
-- path) or both are NULL (legacy rows). Half-populated would be
-- a programming bug; reject at the DB layer.
ALTER TABLE snapshots
    ADD CONSTRAINT snapshots_disk_manifest_both_or_neither
    CHECK (
        (disk_manifest_id IS NOT NULL AND disk_manifest_version IS NOT NULL)
        OR
        (disk_manifest_id IS NULL     AND disk_manifest_version IS NULL)
    );

-- GC scan: "which manifest_ids are live?" is the load-bearing
-- query. A plain btree on (disk_manifest_id) covers it; the
-- partial-index NOT NULL filter keeps the index size proportional
-- to chunked rows only (irrelevant for legacy rows).
CREATE INDEX IF NOT EXISTS idx_snapshots_disk_manifest
    ON snapshots (disk_manifest_id)
    WHERE disk_manifest_id IS NOT NULL;
