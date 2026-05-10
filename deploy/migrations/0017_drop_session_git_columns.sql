-- ADR 0005 / Stage 3: drop the git-shaped columns from `sessions` +
-- the `agent_commits` table. The Rust side dropped `WorkspaceSpec`,
-- `SessionKind`, `Session.workspace`, `Session.session_kind`,
-- `Session.checkpoint_branch`, and `SessionSpec.workspace` in the
-- same change. The platform layer doesn't run a single git command;
-- agents that want to push do it themselves inside the sandbox using
-- credentials mounted via `[secrets.X]`.
--
-- 0006 (`0006_session_kind_and_checkpoint.sql`) added these columns
-- and ADR 0001's auto-checkpoint flow wrote to them. ADR 0002
-- collapsed `pending_reassign` → `dead`. Stage 3 is the final step:
-- the columns are now unused by every code path.

ALTER TABLE sessions
    DROP COLUMN IF EXISTS workspace,
    DROP COLUMN IF EXISTS session_kind,
    DROP COLUMN IF EXISTS checkpoint_branch,
    DROP COLUMN IF EXISTS repo_url;

-- `agent_commits` has been vestigial since 0001 — never populated by
-- production code paths. Phase 4's checkpoint primitive used
-- `session_events` for its `commit_sha` payload, not this table.
DROP TABLE IF EXISTS agent_commits;

-- The `sessions_idle_eviction_idx` partial index keyed off
-- `last_harness_event_at` (added by 0006, dropped by 0007). The
-- `sessions_pending_reassign_idx` partial index referenced the dead
-- `pending_reassign` status string (which 0008 collapsed into
-- `dead`); drop it defensively if it survived.
DROP INDEX IF EXISTS sessions_pending_reassign_idx;
