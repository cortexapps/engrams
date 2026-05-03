-- Phase 5b: registry credentials become polymorphic on auth_kind.
--
-- The 0010 schema assumed every registry row carries a static
-- username + envelope-encrypted password. That model breaks for
-- IAM-vended registries (ECR, GAR with Workload Identity, ACR with
-- AAD) — there's no password the user can paste; the credential is
-- a *claim* about the host's identity that gets exchanged for a
-- short-lived token per pull.
--
-- Reshape: `auth_kind` discriminates the variant, `auth_config`
-- (JSONB) carries variant-specific fields. The cipher quadruple
-- moves into auth_config for `static` rows; cloud-IAM kinds (today
-- `gcp_workload_identity`, future `aws_instance_role` /
-- `gcp_impersonate_sa` / etc.) store no secret material — runtime
-- ambient identity is the credential.
--
-- This migration is destructive: it backfills every existing row as
-- `auth_kind='static'` with its cipher fields packed into JSONB,
-- then drops the now-redundant typed columns. Rollback would
-- re-create the columns and explode the JSONB; not provided.

-- 1. Add new columns (nullable for backfill).
ALTER TABLE registry_credentials
    ADD COLUMN IF NOT EXISTS auth_kind   TEXT,
    ADD COLUMN IF NOT EXISTS auth_config JSONB;

-- 2. Backfill: every existing row becomes 'static', with the cipher
--    quadruple packed into auth_config as base64 strings (JSONB
--    can't hold raw bytea, and base64 round-trips through Postgres
--    + serde_json without lossy charset surprises).
UPDATE registry_credentials
   SET auth_kind   = 'static',
       auth_config = jsonb_build_object(
           'username',    username,
           'wrapped_dek', encode(wrapped_dek, 'base64'),
           'nonce',       encode(nonce,       'base64'),
           'ciphertext',  encode(ciphertext,  'base64'),
           'key_id',      key_id
       )
 WHERE auth_kind IS NULL;

-- 3. Lock down.
ALTER TABLE registry_credentials
    ALTER COLUMN auth_kind   SET NOT NULL,
    ALTER COLUMN auth_config SET NOT NULL;

ALTER TABLE registry_credentials
    ADD CONSTRAINT registry_credentials_auth_kind_check
    CHECK (auth_kind IN ('static', 'gcp_workload_identity'));

-- 4. Drop the now-redundant typed columns. The legacy username
--    column was a candidate for the (host, username) UNIQUE; the
--    polymorphic schema scopes uniqueness to host alone (one
--    credential per host — re-add updates in place).
ALTER TABLE registry_credentials DROP CONSTRAINT IF EXISTS registry_credentials_registry_host_username_key;
ALTER TABLE registry_credentials DROP COLUMN IF EXISTS username;
ALTER TABLE registry_credentials DROP COLUMN IF EXISTS wrapped_dek;
ALTER TABLE registry_credentials DROP COLUMN IF EXISTS nonce;
ALTER TABLE registry_credentials DROP COLUMN IF EXISTS ciphertext;
ALTER TABLE registry_credentials DROP COLUMN IF EXISTS key_id;

ALTER TABLE registry_credentials
    ADD CONSTRAINT registry_credentials_host_unique UNIQUE (registry_host);

COMMENT ON COLUMN registry_credentials.auth_kind IS
    'Discriminator: static | gcp_workload_identity (future: aws_instance_role, gcp_impersonate_sa, ...)';
COMMENT ON COLUMN registry_credentials.auth_config IS
    'Variant-specific JSON payload. For static: {username, wrapped_dek, nonce, ciphertext, key_id} (base64-encoded bytes). For gcp_workload_identity: {impersonate_sa: Option<String>}.';
