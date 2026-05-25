-- ADR 0016 Phase C commit 3a: persist the bake's `disk_manifest`
-- ManifestRef on the enabled_images row so the coord-side chunk-GC
-- can compute its pin set without re-pulling bundle.json from OCI.
--
-- The ref is known at materialize-time in
-- `crates/engram-coordinator/src/api/enabled_images.rs::materialize_disk_chunks`
-- (parsed from bundle.json, bound to the chunks written into
-- BlobStorage at the SAME ref). Persisting it on the row makes
-- Phase C's pin-set query a single PG SELECT — no OCI round-trip,
-- no ImageCache dependency on coord (ImageCache lives in
-- host-agent).
--
-- Columns are NULLable: harness-only images (bake produced just a
-- manifest layer, no chunked-disk artifact — see the
-- `disk_bootstrap_json` / `disk_chunks_blob` `else` branch in
-- materialize_disk_chunks) legitimately have no disk_manifest_ref.
-- The both-or-neither CHECK keeps the (id, version) pair coherent.
--
-- Existing rows from before this migration keep NULL refs. Phase C's
-- rollout is gated on `ENGRAM_CHUNK_GC_ENABLED=0` for ≥1 week of
-- dry-run, by which point operators will have naturally re-enabled
-- chunked-disk images via POST /api/enabled-images (which now
-- populates the new columns). Pin-set gap for the legacy rows is
-- bounded by that window + the 24h grace period.

ALTER TABLE enabled_images
    ADD COLUMN IF NOT EXISTS disk_manifest_id      UUID,
    ADD COLUMN IF NOT EXISTS disk_manifest_version BIGINT;

ALTER TABLE enabled_images
    DROP CONSTRAINT IF EXISTS enabled_images_disk_manifest_both_or_neither;
ALTER TABLE enabled_images
    ADD CONSTRAINT enabled_images_disk_manifest_both_or_neither CHECK (
        (disk_manifest_id IS NULL) = (disk_manifest_version IS NULL)
    );

-- Phase C's pin-set will SELECT DISTINCT (disk_manifest_id,
-- disk_manifest_version) over this table. Index the id column with
-- a partial predicate so harness-only rows (NULL) are excluded from
-- the scan.
CREATE INDEX IF NOT EXISTS idx_enabled_images_disk_manifest
    ON enabled_images (disk_manifest_id)
    WHERE disk_manifest_id IS NOT NULL;
