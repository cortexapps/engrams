-- ADR 0047: two per-pod DashMaps move into PG so any coordinator replica
-- (and a restarted pod) sees them.
--
-- 1. The operator-pinned teleport destination (was state.teleport_targets):
--    written by POST /api/admin/sessions/:id/teleport, read by the evac
--    scanner as its required placement, cleared on resolution/abort.
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS teleport_target_host_id UUID;

-- 2. The per-session credential-broker token (was state.git_broker_tokens):
--    minted once per session, KEK-sealed (same envelope shape as
--    session_secrets), read-through-cached in memory per pod. A guest
--    forge/upload request authorizes on any replica.
CREATE TABLE IF NOT EXISTS session_broker_tokens (
  session_id  UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  wrapped_dek BYTEA NOT NULL,
  nonce       BYTEA NOT NULL,
  ciphertext  BYTEA NOT NULL,
  key_id      TEXT  NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
