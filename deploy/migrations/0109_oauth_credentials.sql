-- ADR 0106: provider-neutral, subject-scoped OAuth connections. The payload is
-- a KEK-sealed opaque provider bundle; only metadata is listable.
CREATE TABLE oauth_credentials (
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('user', 'connector', 'mcp')),
    subject_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    wrapped_dek BYTEA NOT NULL,
    nonce BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    key_id TEXT NOT NULL,
    account_metadata JSONB NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    PRIMARY KEY (subject_kind, subject_id, provider)
);

-- Short-lived coordination only. Verification codes, device codes, raw
-- provider messages, and tokens are deliberately absent.
CREATE TABLE oauth_flows (
    id UUID PRIMARY KEY,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('user', 'connector', 'mcp')),
    subject_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    owner_replica TEXT NOT NULL,
    lease_expires_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'pending', 'succeeded', 'denied', 'cancelled', 'expired',
        'owner_lost', 'failed'
    )),
    error_code TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX oauth_flows_cleanup_idx ON oauth_flows (status, updated_at);
CREATE UNIQUE INDEX oauth_flows_one_pending_per_subject_provider
    ON oauth_flows (subject_kind, subject_id, provider)
    WHERE status = 'pending';

CREATE TABLE session_oauth_bindings (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('user', 'connector', 'mcp')),
    subject_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    FOREIGN KEY (subject_kind, subject_id, provider)
        REFERENCES oauth_credentials(subject_kind, subject_id, provider)
);
