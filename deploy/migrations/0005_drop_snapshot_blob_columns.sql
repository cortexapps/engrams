-- Phase 4 (Track 0): blob storage subsystem retired.
--
-- Snapshot durability moved to local NVMe + git checkpoints (cross-host
-- durability is git, not blob-replicated VM memory). The replication
-- driver, BlobStorage trait, cold_tier_fetch path, and engram-storage-*
-- crates are gone. These columns went with them.

ALTER TABLE snapshots DROP COLUMN IF EXISTS blob_url;
ALTER TABLE snapshots DROP COLUMN IF EXISTS replicated_at;
