-- ADR 0056: per-session integration capabilities.
--
-- The (provider, action, resource) grants a session's profile declared,
-- bound at session create. The broker reads these to clamp every guest
-- credential/action request to what the profile granted ("server decides
-- the scope"). This phase only binds; nothing enforces yet.
--
-- `resource` is the optional scope refinement (a repo glob, a bucket); '' is
-- the no-resource sentinel — NULL can't sit in the primary key, so the Rust
-- side maps '' <-> Option::None. ON DELETE CASCADE: capabilities are
-- meaningless without their session.
CREATE TABLE session_capabilities (
    session_id  UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (session_id, provider, action, resource)
);
