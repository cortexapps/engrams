-- ADR 0051: drop the coordinator's user-identity data model.
--
-- The orchestrator (Bun) now owns all authn/authz and human identity; the
-- coordinator is a pure session/fleet control plane that trusts the
-- orchestrator's bearer-authed app-gRPC. The `users` / `user_tokens` /
-- `web_sessions` tables and the `sessions.user_id` owner column backed the
-- now-deleted `engram-auth` Principal stack and the web-facing REST surface,
-- both of which were removed in this drip. Nothing in the coordinator reads
-- them anymore; attribution lives in the orchestrator's task model.
--
-- Destructive + irreversible. engrams has zero users, so this is a clean
-- break (no data migration / compat shim). `web_sessions` and `user_tokens`
-- reference `users`, so drop them first; `IF EXISTS` keeps the migration
-- idempotent against an already-cut-over database.

DROP TABLE IF EXISTS user_tokens;
DROP TABLE IF EXISTS web_sessions;
DROP TABLE IF EXISTS users;
ALTER TABLE sessions DROP COLUMN IF EXISTS user_id;
