-- Phase 5b: registry-backed images, curated explicitly.
--
-- The model: operators register registries (creds — see
-- registry_credentials), then explicitly enable specific image
-- URIs from those registries. Sessions can only reference enabled
-- URIs; the manifest is fetched from the registry at enable time
-- and stored on the row, so session-create has zero network
-- dependency on the hot path.
--
-- Granularity: full URI (registry host + repo path + tag). No
-- "repo" concept exposed — operators just see and pick images.
-- A new tag of an existing repo is a new enabled-images row.
--
-- Sessions reshape: image_repo + image_tag (Phase 4 (repo, tag)
-- pair) is replaced by image_uri (the full reference). Backfill
-- synthesizes URIs from the legacy columns where possible.

-- Curated allowlist of image URIs that sessions may reference.
CREATE TABLE IF NOT EXISTS enabled_images (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    image_uri          TEXT        NOT NULL UNIQUE,
    -- Full manifest.toml content fetched from the registry at enable
    -- time. Stored verbatim so session-create reads + parses without
    -- a network round-trip. Refreshed via POST /api/enabled-images/<id>/refresh.
    manifest_toml      TEXT        NOT NULL,
    -- sha256 of the manifest layer. Used to short-circuit refresh
    -- when the registry's tag points at the same digest.
    manifest_digest    TEXT        NOT NULL,
    last_refreshed_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_enabled_images_uri ON enabled_images(image_uri);

-- Sessions: image_uri replaces image_repo + image_tag.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS image_uri TEXT;

-- Backfill: combine legacy (repo, tag) into a synthetic URI. For
-- pre-Phase-5 rows that used `local://...` repos, the URI keeps
-- that scheme prefix — those rows aren't pullable from a real
-- registry but at least round-trip through the schema cleanly.
UPDATE sessions
   SET image_uri = image_repo || ':' || image_tag
 WHERE image_uri IS NULL;

ALTER TABLE sessions ALTER COLUMN image_uri SET NOT NULL;

ALTER TABLE sessions DROP COLUMN IF EXISTS image_repo;
ALTER TABLE sessions DROP COLUMN IF EXISTS image_tag;

COMMENT ON COLUMN sessions.image_uri IS
    'Full OCI reference, e.g. ghcr.io/cortex/api:warm-X. Must match an enabled_images.image_uri row at session-create time (404 otherwise).';
COMMENT ON TABLE enabled_images IS
    'Curated allowlist of image URIs. Operators add via POST /api/enabled-images; sessions can only reference enabled URIs.';
