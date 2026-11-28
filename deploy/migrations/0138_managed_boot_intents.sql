-- Immutable approved inputs are committed with the held reservation.
CREATE TABLE session_managed_boot_selections (
    session_id UUID PRIMARY KEY REFERENCES session_managed_creations(session_id) ON DELETE RESTRICT,
    selection JSONB NOT NULL CHECK (jsonb_typeof(selection) = 'object')
);
-- One initial intent and one exact recovery operation per physical source epoch.
CREATE TABLE session_managed_boot_intents (
    intent_id TEXT PRIMARY KEY CHECK (length(intent_id) BETWEEN 1 AND 128),
    session_id UUID NOT NULL REFERENCES session_managed_creations(session_id) ON DELETE RESTRICT,
    operation_key TEXT NOT NULL,
    binding_epoch BIGINT NOT NULL CHECK (binding_epoch BETWEEN 1 AND 9007199254740991),
    revision BIGINT NOT NULL CHECK (revision > 0),
    completed BOOLEAN NOT NULL DEFAULT FALSE,
    record JSONB NOT NULL CHECK (jsonb_typeof(record) = 'object'),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(session_id, operation_key),
    UNIQUE(session_id, binding_epoch)
);
CREATE UNIQUE INDEX session_managed_boot_intents_pending ON session_managed_boot_intents(session_id) WHERE NOT completed;
