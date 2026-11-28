-- Separate from the immutable creation source. A claim precedes all host I/O.
CREATE TABLE session_managed_boots (
    session_id UUID PRIMARY KEY REFERENCES session_managed_creations(session_id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES hosts(id),
    sandbox_id UUID NOT NULL UNIQUE,
    binding_epoch BIGINT NOT NULL CHECK (binding_epoch > 0 AND binding_epoch <= 9007199254740991),
    boot_attempt TEXT NOT NULL UNIQUE CHECK (octet_length(boot_attempt) BETWEEN 1 AND 128),
    revision BIGINT NOT NULL CHECK (revision > 0),
    phase TEXT NOT NULL CHECK (phase IN ('claimed','prepared','finalizing','finalized','start_issued','started','interrupted')),
    record JSONB NOT NULL CHECK (jsonb_typeof(record) = 'object'),
    updated_at TIMESTAMPTZ NOT NULL,
    CHECK (record->>'phase' = phase),
    CHECK ((record->>'revision')::BIGINT = revision)
);
CREATE INDEX session_managed_boots_drive_idx ON session_managed_boots(updated_at, session_id)
    WHERE phase IN ('claimed','finalizing','finalized','start_issued');
CREATE INDEX session_managed_boots_host_idx ON session_managed_boots(host_id);
-- A cold recovery permanently closes delivery targets from the old physical run.
CREATE TABLE session_managed_interrupted_targets (
    session_id UUID NOT NULL REFERENCES session_managed_creations(session_id) ON DELETE CASCADE,
    binding_epoch BIGINT NOT NULL CHECK (binding_epoch > 0),
    boot_attempt TEXT NOT NULL,
    interrupted_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY(session_id, binding_epoch),
    UNIQUE(session_id, boot_attempt)
);
