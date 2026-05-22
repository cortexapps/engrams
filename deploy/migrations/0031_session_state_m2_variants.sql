-- ADR 0015 M2: explicit SessionState machine — extend the
-- `sessions_status_check` CHECK constraint with the three new
-- variants the Rust enum gained.
--
--   created     — sandbox bound to a host; nothing else proven
--   guest_ready — agentd reachable; harness not yet running
--   host_lost   — heartbeat-loss against the bound host
--
-- Pre-M2 the column accepted `pending | active | idle | completed |
-- failed | dead` (set by migration 0020). With M2's atomic insert
-- happening at `created` (not `active`) the prior constraint
-- rejected every cold-create at the DB layer, so this migration
-- has to land in the same deploy as the M2 coord binaries.
--
-- The constraint is rebuilt in place rather than dropped + re-added
-- in two ALTERs so production sees one transactional swap.

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_status_check;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_status_check CHECK (
        status IN (
            'pending',
            'created',
            'guest_ready',
            'active',
            'idle',
            'host_lost',
            'completed',
            'failed',
            'dead'
        )
    );
