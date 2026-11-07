-- Nullable policy preserves unknown legacy authority. Parent identity is immutable.
ALTER TABLE sessions ADD COLUMN mcp_write_policy JSONB
    CHECK (mcp_write_policy IS NULL OR jsonb_typeof(mcp_write_policy) = 'object');
ALTER TABLE sessions ADD COLUMN mcp_policy_parent_id UUID REFERENCES sessions(id);
ALTER TABLE sessions ADD CONSTRAINT mcp_policy_no_self_parent CHECK (mcp_policy_parent_id <> id);
CREATE INDEX sessions_mcp_policy_parent_idx ON sessions(mcp_policy_parent_id)
    WHERE mcp_policy_parent_id IS NOT NULL;
CREATE INDEX session_events_tool_call_idx ON session_events(session_id, (payload->>'tool_call_id'), idx)
    WHERE kind IN ('tool_call_requested', 'tool_result_submitted', 'tool_call_completed');
