CREATE TABLE session_managed_captures (
    capture_id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES session_managed_creations(session_id) ON DELETE CASCADE,
    binding_epoch BIGINT NOT NULL CHECK (binding_epoch > 0),
    source_sandbox_id UUID NOT NULL,
    request JSONB NOT NULL CHECK (jsonb_typeof(request)='object'),
    authority JSONB CHECK (jsonb_typeof(authority)='object'),
    record_digest TEXT UNIQUE CHECK (record_digest ~ '^sha256:[0-9a-f]{64}$'),
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(session_id,binding_epoch),
    CHECK ((authority IS NULL) = (record_digest IS NULL))
);
CREATE INDEX session_managed_captures_source_idx ON session_managed_captures(source_sandbox_id);
