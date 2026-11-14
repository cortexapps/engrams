-- Keep purpose after value deletion. A generic name can never become an MCP ref.
CREATE TABLE org_secret_purposes (
    name text PRIMARY KEY,
    connection_id text,
    role text CHECK (role IN ('api_key', 'client_secret')),
    generation bigint NOT NULL DEFAULT 0 CHECK (generation >= 0),
    CHECK ((connection_id IS NULL) = (role IS NULL)),
    CHECK (connection_id IS NULL OR length(connection_id) > 0)
);
INSERT INTO org_secret_purposes (name) SELECT name FROM org_secrets;
