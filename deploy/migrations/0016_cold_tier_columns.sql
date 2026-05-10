-- ADR 0005 / Phase 6: cold-tier blob durability is reintroduced as
-- the disk-pressure flush target. ADR 0001's `0005_drop_snapshot_
-- blob_columns.sql` retired the original `blob_url` + `replicated_at`
-- columns when blob storage was deleted; this migration adds back
-- the durability columns in their final shape — envelope-encrypted
-- (the sealed blob ref never lands as plaintext) and with a flat
-- `blob_present` flag for cheap LRU/disk-pressure scans.
--
-- Sealed columns mirror the precedent set by `registry_credentials`
-- (0011) and `session_secrets` (0015): per-row DEK encrypted under
-- the deployment KEK; the four-column quartet
-- (`wrapped_dek`, `nonce`, `ciphertext`, `key_id`) is the standard
-- shape `engram-crypto::CredCipher` produces.
--
-- The CHECK constraint says: either we have all four sealed columns
-- (cold copy present), or none of them (no cold copy). Half-populated
-- rows are a bug; reject them at the DB layer rather than letting
-- callers think they have a sealed ref when only some columns are
-- written.
--
-- The partial index supports the disk-pressure detector + cold-resume
-- scheduler scans:
-- * disk-pressure: enumerate snapshots with a hot copy on this host
--   that are also `blob_present` (cheap-drop candidates) — narrow
--   to (host_id, blob_present, local_path).
-- * cold resume: enumerate snapshots with `blob_present=true` for a
--   given session — the session's `latest_cold_snapshot_for_session`.
--
-- `cold_evicted_at` on `sessions` is the timestamp marker for the
-- `Idle → ColdEvicted` transition. Distinct from `last_active_at`
-- (which tracks user activity) and `replicated_at` on `snapshots`
-- (which tracks the blob upload).

ALTER TABLE snapshots
    ADD COLUMN IF NOT EXISTS wrapped_dek   BYTEA,
    ADD COLUMN IF NOT EXISTS nonce         BYTEA,
    ADD COLUMN IF NOT EXISTS ciphertext    BYTEA,
    ADD COLUMN IF NOT EXISTS key_id        TEXT,
    ADD COLUMN IF NOT EXISTS blob_present  BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS replicated_at TIMESTAMPTZ;

ALTER TABLE snapshots
    ADD CONSTRAINT snapshots_sealed_blob_columns_consistent
    CHECK (
        (blob_present     AND wrapped_dek IS NOT NULL
                          AND nonce       IS NOT NULL
                          AND ciphertext  IS NOT NULL
                          AND key_id      IS NOT NULL)
        OR
        (NOT blob_present AND wrapped_dek IS NULL
                          AND nonce       IS NULL
                          AND ciphertext  IS NULL
                          AND key_id      IS NULL)
    );

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS cold_evicted_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_snapshots_residency
    ON snapshots (host_id, blob_present)
    WHERE local_path IS NOT NULL OR blob_present;
