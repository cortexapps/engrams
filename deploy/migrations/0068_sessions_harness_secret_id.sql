-- ADR 0039: persist the harness OAuth secret REFERENCE on the session so the
-- control plane can re-resolve + re-inject it on every resume — including
-- coordinator-driven auto-resume (idle->active, queue scanner) where the
-- orchestrator is not in the request path.
--
-- This stores only the key into `sealed_secrets` (the deployment-KEK-sealed
-- store), never the plaintext. No FK to sealed_secrets: a staged secret may be
-- rotated/deleted out from under the session; resume tolerates a missing ref
-- (logs + continues without the token) exactly as the create-time gate does.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS harness_secret_id TEXT;
