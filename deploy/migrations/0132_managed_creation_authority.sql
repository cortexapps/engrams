-- A held creation is never eligible for ordinary queue placement.
CREATE TABLE session_managed_creations (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    task_id TEXT NOT NULL CHECK (octet_length(task_id) BETWEEN 1 AND 256),
    input_id TEXT NOT NULL UNIQUE CHECK (octet_length(input_id) BETWEEN 1 AND 256),
    actor_id TEXT NOT NULL CHECK (octet_length(actor_id) BETWEEN 1 AND 256),
    parent_session_id UUID REFERENCES sessions(id),
    eligibility TEXT NOT NULL CHECK (eligibility IN ('unknown', 'read_only', 'write_eligible')),
    input_digest TEXT NOT NULL CHECK (input_digest ~ '^sha256:[0-9a-f]{64}$'),
    native_version INTEGER NOT NULL CHECK (native_version = 1),
    native_bytes BYTEA NOT NULL CHECK (octet_length(native_bytes) BETWEEN 1 AND 1048576),
    native_digest TEXT NOT NULL CHECK (native_digest ~ '^sha256:[0-9a-f]{64}$'),
    state TEXT NOT NULL DEFAULT 'held' CHECK (state = 'held'),
    CHECK (parent_session_id <> session_id)
);
CREATE INDEX session_managed_creations_parent_idx ON session_managed_creations(parent_session_id)
    WHERE parent_session_id IS NOT NULL;
CREATE INDEX session_managed_creations_task_idx ON session_managed_creations(task_id);

-- Application retries never update authority. Prevent an accidental SQL upsert too.
CREATE FUNCTION reject_managed_creation_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'managed creation authority is immutable';
END;
$$;
CREATE TRIGGER session_managed_creations_immutable BEFORE UPDATE ON session_managed_creations
    FOR EACH ROW EXECUTE FUNCTION reject_managed_creation_update();
