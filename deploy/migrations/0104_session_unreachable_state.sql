-- ADR 0091: add the `unreachable` SessionState — "the coordinator
-- believes a sandbox exists, but its guest is not responding on the
-- control plane." Producer: the host-agent's periodic-checkpoint driver
-- after N consecutive socket-level failures against the FC API socket
-- (advertised in the heartbeat); consumer: ensure_active auto-drives the
-- existing checkpoint recovery on the next prompt/exec instead of
-- forwarding into a dead VM. Pre-ADR, a dead guest read `active`
-- indefinitely (2026-07-11 campaign C1: 16+ min of a zombie session
-- only in-guest execs could unmask).
--
-- The 0064/0077 drop/re-add pattern.
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN (
    'pending', 'queued', 'created', 'active', 'idle', 'unreachable',
    'host_lost', 'evacuating', 'evicting', 'completed', 'failed', 'dead'
));
