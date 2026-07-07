-- ADR 0077 phase 3: a session's boot inputs as ONE persisted document,
-- so create / queue re-prepare / resume / evac all consume the same
-- spec instead of re-deriving it (the re-derivation class: the
-- queued-skills TODO(P1-D), the forge-token FK-ordering shape,
-- post-resume egress drift). serde-versioned JSONB.
--
-- Numbered 0089: follows this PR's 0088 (durable_head).
CREATE TABLE session_runtime_specs (
    session_id  UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    spec        JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
