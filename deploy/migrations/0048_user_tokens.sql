-- Per-user sealed credentials (ADR 0031).
--
-- Today the only kind is 'claude_oauth': the Claude Code OAuth token a user
-- saves once on their profile, which the coordinator auto-injects into
-- built-in-Claude sessions so we never prompt per session again.
--
-- Envelope-encrypted exactly like `session_secrets` (migration 0015) and
-- `registry_credentials`: a per-row DEK encrypts the plaintext token, and
-- the DEK itself is wrapped under the deployment's master key (KEK). The
-- row holds ciphertext only; the coordinator never logs the plaintext and
-- never echoes the token on any API (the profile shows only whether one is
-- saved). Decryption happens at session-create, where the plaintext is
-- handed straight to the harness env.
--
-- PRIMARY KEY (user_id, kind): one token per (user, kind); saving again
-- replaces it. ON DELETE CASCADE drops a user's tokens when the user is
-- deprovisioned.
CREATE TABLE IF NOT EXISTS user_tokens (
    user_id      UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind         TEXT        NOT NULL DEFAULT 'claude_oauth',
    wrapped_dek  BYTEA       NOT NULL,
    nonce        BYTEA       NOT NULL,
    ciphertext   BYTEA       NOT NULL,
    key_id       TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ,
    PRIMARY KEY (user_id, kind)
);
