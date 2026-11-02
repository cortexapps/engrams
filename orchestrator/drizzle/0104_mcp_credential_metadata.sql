-- Coordinator receipts and serialized transition metadata only; no credential values.
ALTER TABLE integration_connection ADD COLUMN mcp_credential jsonb NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE integration_connection ADD CONSTRAINT integration_connection_mcp_metadata_object CHECK (jsonb_typeof(mcp_credential) = 'object');
