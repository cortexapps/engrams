-- Phase 5: Docker-registry-backed image and harness distribution.
--
-- Coordinator stops being a stateful image server. Instead:
--   - Images live in a Docker registry (any OCI Distribution Spec
--     registry: GHCR, GCR, ECR, Harbor, local registry:2).
--   - Harness packs live in the same registry (different mediaType).
--   - Registry credentials live encrypted in `registry_credentials`
--     using envelope encryption (per-cred AES-256-GCM DEK wrapped by
--     a deployment KEK held outside Postgres).
--
-- The pre-existing `image_versions.blob_url` column (Phase 4
-- placeholder) is repurposed to hold the OCI URI. NULL keeps its old
-- meaning during rollout (legacy on-disk image).

-- One row per (registry_host, username) pair. The password is sealed
-- under envelope encryption: `wrapped_dek` is the per-row DEK wrapped
-- by the active KEK; `nonce` is the per-row AES-GCM nonce; `ciphertext`
-- is AES-256-GCM(DEK, nonce, password); `key_id` records which KEK
-- generation produced the wrapped_dek so we can detect rotation.
CREATE TABLE IF NOT EXISTS registry_credentials (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    registry_host TEXT        NOT NULL,
    username      TEXT        NOT NULL,
    wrapped_dek   BYTEA       NOT NULL,
    nonce         BYTEA       NOT NULL,
    ciphertext    BYTEA       NOT NULL,
    key_id        TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ,
    UNIQUE (registry_host, username)
);

CREATE INDEX IF NOT EXISTS idx_registry_creds_host
    ON registry_credentials(registry_host);

-- Pointer-only registry of harness packs. The actual pack contents
-- live in the OCI registry at `registry_uri`. `name` is the unique
-- key sessions select by (`HarnessSpec::Pack { name: "claude" }`).
CREATE TABLE IF NOT EXISTS harness_packs (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    name         TEXT        NOT NULL UNIQUE,
    registry_uri TEXT        NOT NULL,
    description  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ
);

-- Repurpose the existing nullable column. NULL = legacy on-disk image
-- (Phase 4 fallback path); non-NULL = OCI URI in a Docker registry.
COMMENT ON COLUMN image_versions.blob_url IS
    'OCI registry URI (e.g. gcr.io/cortex/api:warm-X). NULL = legacy on-disk image.';
