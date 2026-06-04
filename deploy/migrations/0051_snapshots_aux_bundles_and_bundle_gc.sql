-- ADR 0035: content-addressed bundle generations, pinned by GC.
--
-- `snapshots.aux_bundles` records which bundle generations a snapshot's
-- device model references, as `[{"drive_id": "...", "sha256": "..."}]`.
-- The DISTINCT union across all rows is the bundle-GC pin set: a
-- generation in BlobStorage (`bundles/sha256/<sha>`) or a host staging
-- dir is live iff some snapshot row references it.
--
-- Existing rows default to '[]' — every pre-0051 snapshot is already
-- unrestorable bundle-wise (the 2026-06-03 skew incident), and the
-- remediation re-captures base snapshots anyway.
ALTER TABLE snapshots
    ADD COLUMN aux_bundles jsonb NOT NULL DEFAULT '[]'::jsonb;

-- Mirror of chunk_gc_candidates for bundle generations: unpinned blob
-- keys park here with a sticky first_seen_at; the promote pass deletes
-- from BlobStorage after the grace period. sha256 is the 64-hex digest
-- (the blob key suffix), text rather than bytea — bundle counts are
-- tiny (a handful of generations), readability beats compactness.
CREATE TABLE bundle_gc_candidates (
    sha256        text PRIMARY KEY,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);
