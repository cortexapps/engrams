-- MCP encrypted redirect state stays with the existing flow authority.
ALTER TABLE oauth_flows ADD COLUMN mcp_state jsonb;
ALTER TABLE oauth_flows ADD CONSTRAINT mcp_state_subject CHECK (mcp_state IS NULL OR subject_kind='mcp');
ALTER TABLE oauth_credentials ADD COLUMN mcp_refresh_claim uuid;
CREATE TABLE mcp_oauth_admission (
    deployment_id text NOT NULL,
    slot smallint NOT NULL CHECK (slot BETWEEN 0 AND 3),
    id uuid NOT NULL UNIQUE,
    connection_id text NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (deployment_id, slot),
    UNIQUE (deployment_id, connection_id)
);
CREATE INDEX mcp_oauth_admission_expiry ON mcp_oauth_admission (expires_at);
