-- ADR 0080 Phase 2a: consolidated ImageConfig replaces the baked
-- manifest + the capture_env side-channel.
--
-- All image metadata (name/description/env/workdir/resources/warm)
-- now arrives via the ImageService RPC as one `image_config` JSONB
-- document; the Dockerfile's ENV/WORKDIR are extracted from the OCI
-- config blob at materialize time into `oci_defaults`. The baked
-- `manifest_toml` (engram.toml) and the standalone `capture_env`
-- columns retire. WarmConfig absorbs capture env (`warm.env`) and
-- capture egress (`warm.network`) — everything warm is
-- recapture-affecting by definition.
--
-- Zero users (pre-GA): wipe both tables instead of backfilling.
-- Rollout quiesces sessions and re-enables each image once via the
-- new RPC, supplying its config out-of-band.

DELETE FROM enable_jobs;
DELETE FROM enabled_images;

ALTER TABLE enabled_images
    DROP COLUMN manifest_toml,
    DROP COLUMN capture_env,
    ADD COLUMN image_config JSONB NOT NULL,
    ADD COLUMN oci_defaults JSONB NOT NULL;

ALTER TABLE enable_jobs
    DROP COLUMN capture_env,
    ADD COLUMN image_config JSONB NOT NULL;

COMMENT ON COLUMN enabled_images.image_config IS
    'The operator-supplied ImageConfig (name/description/env/workdir/resources/warm incl. warm.env + warm.network) this row''s base snapshot was captured under. Cheap fields (name/description/env/workdir) may be updated in place via UpdateImage; capture-affecting fields only change through a recapture job.';
COMMENT ON COLUMN enabled_images.oci_defaults IS
    'OciRuntimeDefaults (ENV/WORKDIR) extracted from the image''s OCI config blob at materialize time. Merged under image_config at session create (config wins).';
COMMENT ON COLUMN enable_jobs.image_config IS
    'The ImageConfig this enable job captures under (carried from the triggering enable/update request, or inherited from the enabled_images row on refresh). Stamped onto enabled_images only when the job reaches ready, so capture-affecting edits stay invisible until the new base snapshot exists.';
