-- Decouple image / workspace / harness as orthogonal session-time
-- axes. Replaces the conflated `repo: String` field that previously
-- encoded both "where the workspace comes from" (`git+...` /
-- `local://`) and the registry lookup key, plus the global
-- `dev_auto_agent` config that picked the harness coord-wide.
--
--   image      → ImageRef::Registry { repo, tag }   (image_repo + image_tag)
--   workspace  → WorkspaceSpec discriminated JSON   (workspace JSONB)
--   harness    → HarnessSpec   discriminated JSON   (harness   JSONB)
--
-- Backwards compat is intentionally out of scope (nothing is shipped).
-- We backfill new columns from the legacy `repo`/`branch`/`repo_url`
-- so dev DBs with stale rows survive the migration, then drop the
-- legacy columns and tighten the new ones to NOT NULL.

-- 1. Add new columns (nullable for the backfill window).
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS image_repo TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS image_tag  TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS workspace  JSONB;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS harness    JSONB;

-- 2. Backfill `image_repo` / `image_tag` from the legacy `repo` /
--    `image_version` pair. The legacy `repo` field doubled as the
--    registry label, so use it directly.
UPDATE sessions
   SET image_repo = repo,
       image_tag  = image_version
 WHERE image_repo IS NULL;

-- 3. Backfill `workspace` JSON from `repo_url` + `branch` + the
--    derived read-only flag (`session_kind = 'readonly'`).
--    `git+...` → {kind:"git", url, branch, read_only}
--    `local://...` → {kind:"empty"}    (label discarded — `local://`
--                                       was never a real workspace)
--    everything else (NULL repo_url, malformed) → {kind:"empty"} as
--    a defensive default; the row was unusable anyway.
UPDATE sessions
   SET workspace = CASE
       WHEN repo_url LIKE 'git+%' THEN
           jsonb_build_object(
               'kind',      'git',
               'url',       substring(repo_url FROM 5),
               'branch',    branch,
               'read_only', (session_kind = 'readonly')
           )
       ELSE
           jsonb_build_object('kind', 'empty')
   END
 WHERE workspace IS NULL;

-- 4. Backfill `harness` JSON. Pre-migration sessions have no harness
--    record on the row (it lived in coord config); default to none.
UPDATE sessions
   SET harness = jsonb_build_object('kind', 'none')
 WHERE harness IS NULL;

-- 5. Migrate `session_kind = 'local'` to `'ephemeral'` and tighten
--    the constraint to the new variant set. The DROP-before-UPDATE
--    order matters: the legacy CHECK from 0006 forbids 'ephemeral',
--    so the UPDATE must run with the old constraint already gone.
--    Also drop the legacy `DEFAULT 'local'` set by 0006 — it would
--    now violate the new constraint, and every insert path supplies
--    the kind explicitly anyway.
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_session_kind_check;
ALTER TABLE sessions ALTER COLUMN session_kind DROP DEFAULT;
UPDATE sessions SET session_kind = 'ephemeral' WHERE session_kind = 'local';
ALTER TABLE sessions ADD CONSTRAINT sessions_session_kind_check
    CHECK (session_kind IN ('git', 'readonly', 'ephemeral'));

-- 6. Lock down the new columns.
ALTER TABLE sessions ALTER COLUMN image_repo SET NOT NULL;
ALTER TABLE sessions ALTER COLUMN image_tag  SET NOT NULL;
ALTER TABLE sessions ALTER COLUMN workspace  SET NOT NULL;
ALTER TABLE sessions ALTER COLUMN harness    SET NOT NULL;

-- 7. Drop the legacy columns. `repo` was the conflated workspace+image
--    label; `repo_url` was its parsed form; `image_version` is now
--    `image_tag`; `branch` lived inside `workspace.git.branch`.
ALTER TABLE sessions DROP COLUMN IF EXISTS repo;
ALTER TABLE sessions DROP COLUMN IF EXISTS branch;
ALTER TABLE sessions DROP COLUMN IF EXISTS image_version;
ALTER TABLE sessions DROP COLUMN IF EXISTS repo_url;
