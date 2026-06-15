-- ADR 0039: persist the initiating user's git-attribution identity on the
-- session so the control plane re-injects ENGRAM_USER_EMAIL / ENGRAM_USER_NAME
-- on every resume (incl. coordinator-driven auto-resume, where the orchestrator
-- is not in the loop). engram-session-bundles reads these to write the guest
-- /etc/gitconfig [user] block so the agent's commits stay attributed.
--
-- Attribution metadata only — NOT an authz identity. Distinct from the dropped
-- user_id (0067): no FK, no role, never consulted for access control.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS user_email TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS user_name TEXT;
