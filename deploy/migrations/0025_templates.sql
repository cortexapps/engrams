-- ADR 0014: warm-pool template snapshots.
--
-- Each row maps (image_repo, image_tag, harness_pack_uri) to the
-- portable snapshot artifact the image-builder produced at bake
-- time. The host-agent's warm-pool refill loop reads these to know
-- which templates it should pre-restore microVMs for; the coord
-- scheduler resolves a session's `(image, harness)` to the matching
-- `template_ref` before parallel-asking hosts for a warm slot.
--
-- Snapshot artifacts (state.bin, sidecar.json, memory manifest) live
-- in BlobStorage keyed by `snapshot_id` (snapshots row). On rebake,
-- a new row lands and the prior row's `active` flips to FALSE — the
-- 60s grace period in the host's warm-pool driver keeps old-ref
-- slots alive long enough to drain before the refill loop reclaims
-- them.

CREATE TABLE IF NOT EXISTS templates (
    template_ref     UUID         PRIMARY KEY,
    image_repo       TEXT         NOT NULL,
    image_tag        TEXT         NOT NULL,
    harness_pack_uri TEXT         NOT NULL,
    snapshot_id      UUID         NOT NULL REFERENCES snapshots(id),
    vcpus            INT          NOT NULL,
    memory_mib       INT          NOT NULL,
    created_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    -- TRUE for the most recent bake of this (repo, tag, harness)
    -- tuple. Updated to FALSE when a new template_ref lands for
    -- the same triple.
    active           BOOLEAN      NOT NULL DEFAULT TRUE,
    UNIQUE (image_repo, image_tag, harness_pack_uri, snapshot_id)
);

-- Hosts look up "active templates" by repo+tag+harness via this
-- index; coord scheduler does the same when resolving a session
-- spec to a template_ref candidate.
CREATE INDEX IF NOT EXISTS templates_active_lookup
    ON templates (image_repo, image_tag, harness_pack_uri)
    WHERE active = TRUE;
