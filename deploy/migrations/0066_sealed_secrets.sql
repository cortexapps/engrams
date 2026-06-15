-- ADR 0039 Task 13: KEK-sealed opaque secret store for orchestrator secrets.
-- Keys are caller-supplied (orchestrator uses its better-auth user ids).
-- The sealing shape (wrapped_dek / nonce / ciphertext / key_id) mirrors
-- the existing user_tokens and session_secrets tables.
CREATE TABLE IF NOT EXISTS sealed_secrets (
    key         TEXT        PRIMARY KEY,
    wrapped_dek BYTEA       NOT NULL,
    nonce       BYTEA       NOT NULL,
    ciphertext  BYTEA       NOT NULL,
    key_id      TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ
);
