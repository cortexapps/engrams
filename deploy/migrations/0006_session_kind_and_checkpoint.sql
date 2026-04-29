-- Phase 4 (Track C): session-kind + checkpoint plumbing.
--
-- Adds the columns the coord populates at session-create time
-- (parsed from `repo` + `read_only`) and that the checkpoint /
-- resume paths read. `session_kind` defaults to 'local' so legacy
-- rows inserted before this migration round-trip without surprise;
-- new rows always set the kind explicitly.

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS session_kind TEXT NOT NULL DEFAULT 'local';
   -- 'git' (writable) | 'local' (ephemeral) | 'readonly' (clones, never pushes)
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS repo_url TEXT;
   -- Canonical parsed form: `git+https://...` | `git+ssh://...` | `local://<name>`.
   -- NULL on legacy rows; populated for new rows by the coord at create time.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS checkpoint_branch TEXT;
   -- 'engram/sessions/<id>' for SessionKind::Git; NULL otherwise.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_harness_event_at TIMESTAMPTZ;
   -- Wall-clock of the most recent harness event the host received for
   -- this session. Idle evictor (Track B) reads this to decide who's
   -- safe to hot-suspend.

-- Constrain session_kind to the known set so a typo at the API
-- boundary fails fast instead of silently widening the value space.
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_session_kind_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_session_kind_check
    CHECK (session_kind IN ('git', 'local', 'readonly'));

-- Idle evictor wants a fast scan of "active sessions past their TTL";
-- a partial index on last_harness_event_at scoped to active rows
-- keeps that O(idle-count) rather than O(all-active).
CREATE INDEX IF NOT EXISTS sessions_idle_eviction_idx
    ON sessions (last_harness_event_at)
    WHERE status = 'active';

-- Auto-resume reaper (Track D) wants the "ready to come back" pile.
CREATE INDEX IF NOT EXISTS sessions_pending_reassign_idx
    ON sessions (status)
    WHERE status = 'pending_reassign';
