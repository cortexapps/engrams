-- ADR 0028 addendum (2026-06-04): portable snapshot blobs are pinned by GC.
--
-- The per-snapshot portable artifacts under `snapshots/<id>/`
-- (`state.bin`, `sidecar.json`, and the dormant `rootfs.tar.zst` /
-- `working_set.json`) were the only durable resource class NOT governed
-- by the pin-set GC model that owns chunks (`chunk_gc`) and bundles
-- (`bundle_gc`). They were instead managed by the host's inline
-- `abort_prior_inflight_snapshot`, which blind-deleted them with no PG
-- check — so a concurrent producer (periodic checkpoint vs eviction)
-- could delete a snapshot whose row was already recorded
-- `recoverable = true`, bricking resume. This table brings them under
-- the same pin-set + grace + barrier model.
--
-- Liveness: `snapshots/<id>/...` is pinned iff a row with `<id>` exists
-- in `snapshots` (ANY row — session snapshots, checkpoints, AND
-- `session_id IS NULL` template/base captures referenced by
-- `enabled_images.base_snapshot_id`). NOT gated on `recoverable` (the
-- self-heal demotes rows transiently) nor `session_id` (base rows are
-- never deleted: `prune_session_snapshots` is `session_id IS NOT NULL`
-- only, and the `base_snapshot_id` FK has no `ON DELETE`). The snapshot
-- pin set is therefore `SELECT id FROM snapshots`.
--
-- Mirror of chunk_gc_candidates / bundle_gc_candidates: an unpinned
-- snapshot id parks here with a sticky first_seen_at; the promote pass
-- deletes its blobs from BlobStorage after the grace period. The natural
-- key is the snapshot UUID, so `uuid` rather than text.
CREATE TABLE snapshot_blob_gc_candidates (
    snapshot_id   uuid PRIMARY KEY,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_snapshot_blob_gc_candidates_first_seen
    ON snapshot_blob_gc_candidates (first_seen_at);
