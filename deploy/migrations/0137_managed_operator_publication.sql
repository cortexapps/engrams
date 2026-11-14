-- The gate is locked before these records and all session rows.
CREATE TABLE managed_operator_publications (
    generation BIGINT PRIMARY KEY CHECK (generation > 0 AND generation <= 9007199254740991),
    record JSONB NOT NULL,
    published BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE UNIQUE INDEX managed_operator_one_pending ON managed_operator_publications ((TRUE)) WHERE NOT published;
CREATE TABLE managed_operator_readiness (
    generation BIGINT NOT NULL REFERENCES managed_operator_publications(generation),
    session_id UUID NOT NULL REFERENCES sessions(id),
    participant JSONB NOT NULL,
    proof JSONB,
    PRIMARY KEY (generation, session_id)
);
CREATE INDEX session_managed_policy_participants ON session_integration_policy(session_id)
    WHERE policy_json::jsonb->'managed_deny' IS NOT NULL AND policy_json::jsonb->'managed_deny' <> 'null'::jsonb;
