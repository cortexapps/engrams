-- ADR 0081 phase 3: cold_bases needs a full, restorable SnapshotMetadata,
-- not just the (disk_manifest, memory_manifest) text pair migration 0095
-- shipped. Restoring a cold base means calling
-- `SandboxBackend::restore(SnapshotMetadata)` — a real struct carrying
-- `state_blob_key`/`sidecar_blob_key`/`image_version`/`size_bytes`/etc,
-- most of which happen to be deterministically derivable from
-- `snapshot_id` alone, but not all (e.g. `aux_bundles`, `paused_at`).
-- Rather than reconstruct a lossy approximation from the sparse columns,
-- store the executor's own bincode-encoded `SnapshotMetadata` verbatim —
-- the exact value it captured, byte-identical to what a real restore
-- needs.

ALTER TABLE cold_bases ADD COLUMN snapshot_bincode BYTEA;
