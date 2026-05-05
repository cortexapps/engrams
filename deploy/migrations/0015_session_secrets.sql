-- Per-session per-request secrets, sealed under the deployment KEK.
--
-- Why: when a session goes Idle and then resumes (auto on next prompt
-- or via /resume), the in-VM bootstrap respawns the harness as a
-- fresh child. The previous harness's env — including the OAuth /
-- API-key tokens the user pasted into the dashboard at create time —
-- is gone. Without persistence, the resumed harness boots with an
-- empty secret env and Claude (or any auth-bearing harness) prompts
-- the user to log in again.
--
-- These rows are envelope-encrypted just like `registry_credentials`:
-- a per-row DEK wraps the JSON-serialized secret map, and the DEK
-- itself is wrapped under the deployment's master key (KEK). The
-- coordinator never logs the plaintext, never echoes it on
-- session-list responses, and only opens it on the resume path
-- (where it's immediately handed to the in-VM bootstrap as launch
-- env).
--
-- ON DELETE CASCADE on session_id: when a session row is deleted
-- (operator wipe, manual cleanup), its secrets vanish too. We don't
-- own a session-row delete code path today (status transitions
-- handle terminal states), but if we add one later this constraint
-- protects against orphan secret rows.
CREATE TABLE IF NOT EXISTS session_secrets (
    session_id   UUID        PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    wrapped_dek  BYTEA       NOT NULL,
    nonce        BYTEA       NOT NULL,
    ciphertext   BYTEA       NOT NULL,
    key_id       TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
