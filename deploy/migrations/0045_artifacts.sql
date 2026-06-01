-- ADR 0026: session file artifacts. A file shared into a session
-- (agent screenshot/recording via the untrusted in-guest `share-file`
-- skill, or an operator pull of any file by path) is streamed into
-- object storage under the GC-safe `artifacts/<session_id>/<id>` prefix
-- and recorded here. The row surfaces a `file_shared` session event and
-- backs the serve endpoint `GET /sessions/:id/artifacts/:artifact_id`.
--
-- `media_type` is the coord-DETECTED type (magic-byte sniff), never the
-- guest-supplied one. `id` is server-generated and also names the blob
-- key, so the guest never influences the storage path. Blobs live
-- forever (chunk-GC never enumerates the `artifacts/` prefix); the row
-- cascades on session delete (the blob is intentionally left — see ADR).

CREATE TABLE IF NOT EXISTS artifacts (
    id          UUID PRIMARY KEY,
    session_id  UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    blob_key    TEXT NOT NULL,
    media_type  TEXT NOT NULL,
    size_bytes  BIGINT NOT NULL,
    caption     TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Serve lookup is `WHERE id = $1 AND session_id = $2` (PK covers id);
-- the per-session quota check + any future listing hit `session_id`.
CREATE INDEX IF NOT EXISTS artifacts_session_idx ON artifacts(session_id);
