-- ADR 0020 P1: base snapshots — per-image bake-time FC snapshots that the
-- create path restores from instead of cold-booting a fresh kernel.
--
-- The image-builder boots the freshly-baked rootfs under Firecracker (with a
-- stub harness attached, harness unmounted — ADR 0014 option-D capture point),
-- pauses, snapshots, chunks memory.bin into the chunk store, and uploads
-- state.bin + sidecar.json to BlobStorage. The resulting snapshot is recorded
-- in the `snapshots` table (session_id = NULL — it's a template artifact, not a
-- session capture; the column was made nullable in 0028) with recoverable=true.
--
-- This table maps an enabled image's `manifest_digest` (the same digest hosts
-- report in their heartbeat `ready_images`, and that `create_session_inner`
-- already has via `enabled.manifest_digest`) to that base snapshot. On
-- POST /sessions, the create handler looks up the base snapshot by digest and,
-- on a hit, routes through the prod-validated restore() path; on a miss it
-- falls back to a cold create. This is ADR 0014's M1.11 cascade retargeted
-- from the dropped `templates` table (0030) onto a digest-keyed lookup.
--
-- The portable blob keys (state.bin / sidecar.json / working_set.json) are
-- deterministic from `snapshot_id` (`snapshots/<id>/...`), so they are NOT
-- stored here — the restore path recomputes them. The disk + memory chunked
-- manifests live on the `snapshots` row. This table carries only the digest →
-- snapshot mapping plus the resource hints the scheduler needs for capacity fit.

CREATE TABLE IF NOT EXISTS base_snapshots (
    -- The image manifest digest; the natural key. One base snapshot per
    -- enabled image. Re-enabling / rebaking the same digest is idempotent
    -- (ON CONFLICT updates the snapshot pointer + resource hints).
    manifest_digest  TEXT         PRIMARY KEY,
    snapshot_id      UUID         NOT NULL REFERENCES snapshots(id),
    image_repo       TEXT         NOT NULL,
    image_tag        TEXT         NOT NULL,
    -- Resource hints copied from the bake so the scheduler can capacity-fit
    -- a restore without re-parsing the image manifest.
    vcpus            INT          NOT NULL,
    memory_mib       INT          NOT NULL,
    created_at       TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Operators / dashboards list base snapshots by image identity.
CREATE INDEX IF NOT EXISTS base_snapshots_image_lookup
    ON base_snapshots (image_repo, image_tag);
