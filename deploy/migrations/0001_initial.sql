-- Engram v0.1 schema. See DESIGN.md "Database schema (Postgres)".

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS hosts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    hostname TEXT NOT NULL UNIQUE,
    cloud_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    capacity_total_gb INT NOT NULL,
    capacity_used_gb INT NOT NULL,
    last_heartbeat_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL,                    -- ready | draining | dead
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    user_id TEXT,
    status TEXT NOT NULL,                    -- pending | active | idle | completed | failed
    image_version TEXT NOT NULL,
    host_id UUID REFERENCES hosts(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_active_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS snapshots (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    host_id UUID REFERENCES hosts(id) ON DELETE SET NULL,
    local_path TEXT,
    blob_url TEXT,
    image_version TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    replicated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    idx INT NOT NULL,
    role TEXT NOT NULL,                      -- user | assistant | tool
    content_json JSONB NOT NULL,
    tokens INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ,
    UNIQUE (session_id, idx)
);

CREATE TABLE IF NOT EXISTS tool_calls (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    message_id UUID NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    input_json JSONB NOT NULL,
    output_json JSONB,
    exit_status INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS agent_commits (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    branch TEXT NOT NULL,
    pushed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS image_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo TEXT NOT NULL,
    tag TEXT NOT NULL,                       -- warm-<timestamp>
    blob_url TEXT,
    status TEXT NOT NULL,                    -- building | ready | retired
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ,
    UNIQUE (repo, tag)
);

CREATE INDEX IF NOT EXISTS idx_snapshots_session ON snapshots(session_id);
CREATE INDEX IF NOT EXISTS idx_snapshots_host ON snapshots(host_id);
CREATE INDEX IF NOT EXISTS idx_messages_session_idx ON messages(session_id, idx);
CREATE INDEX IF NOT EXISTS idx_sessions_status
    ON sessions(status) WHERE status IN ('pending', 'active', 'idle');
CREATE INDEX IF NOT EXISTS idx_image_versions_repo_status
    ON image_versions(repo, status);
