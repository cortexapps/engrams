-- ADR 0115: admit the `user_connector` subject kind — a user's personal
-- connector credential (PAT or OAuth), keyed by the orchestrator user id.
-- The 0109 tables hard-code the kind list in inline CHECK constraints.

ALTER TABLE oauth_credentials
    DROP CONSTRAINT oauth_credentials_subject_kind_check;
ALTER TABLE oauth_credentials
    ADD CONSTRAINT oauth_credentials_subject_kind_check
    CHECK (subject_kind IN ('user', 'connector', 'mcp', 'user_connector'));

ALTER TABLE oauth_flows
    DROP CONSTRAINT oauth_flows_subject_kind_check;
ALTER TABLE oauth_flows
    ADD CONSTRAINT oauth_flows_subject_kind_check
    CHECK (subject_kind IN ('user', 'connector', 'mcp', 'user_connector'));

ALTER TABLE session_oauth_bindings
    DROP CONSTRAINT session_oauth_bindings_subject_kind_check;
ALTER TABLE session_oauth_bindings
    ADD CONSTRAINT session_oauth_bindings_subject_kind_check
    CHECK (subject_kind IN ('user', 'connector', 'mcp', 'user_connector'));
