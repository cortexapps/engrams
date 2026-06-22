-- ADR 0055 P2: the org-shared user-uploaded skill catalog.
--
-- P1 leaned on the fleet's baked `current_bundles` stamp as the (admin) skill
-- catalog; user uploads can't be baked into the host image, so P2 adds this
-- durable catalog of content-addressed skill bundles. The session-create
-- resolver resolves a selected skill NAME against `fleet_stamp ∪ mount_catalog`;
-- the live shas here fold into `bundle_pin_set()` so every host auto-stages an
-- uploaded skill via the existing materialize-by-sha path, and a soft-delete
-- drops it from the pin set so the existing bundle GC reclaims the blob after
-- the grace window (upload-path GC — no new mechanism).
--
-- `owner` is attribution / GC ownership / quota — NOT an access boundary: the
-- catalog is org-shared, so any (admin-curated, shared) profile may select any
-- skill. `name` is the logical bundle name used verbatim in `profile.skills`
-- and on the wire; it is UNIQUE among live rows, and (enforced in the
-- coordinator at registration) forbidden from colliding with a fleet bundle
-- name. `mount_json` is the manifest baked into the squashfs root, kept for the
-- record — the hot path never re-parses it (the manifest already rides inside
-- the mounted bundle, read by `activate()`).

CREATE TABLE mount_catalog (
    id          TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    owner       TEXT        NOT NULL,
    name        TEXT        NOT NULL,
    description TEXT        NOT NULL DEFAULT '',
    sha256      TEXT        NOT NULL,
    mount_json  TEXT        NOT NULL,
    size_bytes  BIGINT      NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at  TIMESTAMPTZ
);

-- One live row per name (a soft-delete frees the name for re-registration);
-- also the `ON CONFLICT (name) WHERE deleted_at IS NULL` upsert target.
CREATE UNIQUE INDEX mount_catalog_live_name ON mount_catalog (name) WHERE deleted_at IS NULL;

-- The pin-set union + GC scans hit live rows by content.
CREATE INDEX mount_catalog_live_sha ON mount_catalog (sha256) WHERE deleted_at IS NULL;
