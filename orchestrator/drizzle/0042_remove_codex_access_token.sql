-- ADR 0106: Codex human sessions use the coordinator-owned OAuth connection.
-- Delete the obsolete per-user token rows only after the additive OAuth path
-- and descriptor flip are present in this release.
DELETE FROM "user_session_secrets"
WHERE "env_var_name" = 'CODEX_ACCESS_TOKEN';
