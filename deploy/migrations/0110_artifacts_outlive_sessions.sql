-- Artifacts outlive sessions. The cross-session artifact registry
-- (orchestrator-side) references coordinator artifact rows by id, so a
-- session delete must not cascade away the byte-lookup row — the blob
-- already lives forever under the GC-exempt `artifacts/` prefix, and a
-- row without its blob-key metadata is unreachable.
--
-- `file_name` records the sanitized guest-declared basename for
-- `Content-Disposition` and display. NULL for artifacts shared before
-- this migration (serving falls back to `<id>.<ext>`).

ALTER TABLE artifacts DROP CONSTRAINT IF EXISTS artifacts_session_id_fkey;
ALTER TABLE artifacts ADD COLUMN IF NOT EXISTS file_name TEXT;
