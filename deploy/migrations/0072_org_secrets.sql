-- ADR 0057 (A1): admin-managed, KEK-sealed org secret store.
--
-- Mirrors the session_broker_tokens envelope (wrapped_dek + nonce + ciphertext
-- + key_id), keyed by a flat `name` instead of a session id. Resolved through
-- the composed SecretStore (the org backend layered ahead of the deployment
-- backend) — so profile secrets, integration inject creds, and the GitHub App
-- mint key all read from here. The row holds ciphertext only; the value never
-- transits the orchestrator wire (sealed at the coordinator on write).
--
-- upsert/delete fire pg_notify('org_secret_changed', name) so every replica's
-- mint broker invalidates its cached engine on a rotation (ADR 0057 C2).
CREATE TABLE org_secrets (
    name        TEXT PRIMARY KEY,
    wrapped_dek BYTEA       NOT NULL,
    nonce       BYTEA       NOT NULL,
    ciphertext  BYTEA       NOT NULL,
    key_id      TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
