-- Issue #530 (generation purge): drop the `guest_ready` SessionState.
--
-- `guest_ready` was ADR 0015 M2's placeholder for a future split of the
-- ready-probe RPC from the harness-spawn RPC. `start_agent` has always done
-- both in one call, so the create path collapses Created -> GuestReady ->
-- Active into a single UPDATE — the variant has never persisted on the
-- normal path. No future split ever landed.
--
-- Backfill first (defensive — the state "never persists" per its own
-- doc, but treat that as unverified): any row somehow still sitting in
-- guest_ready reverts to created, which is FSM-legal (created -> active
-- is still a valid edge) and lets the create/resume path re-drive it
-- forward exactly as if the ready-dial hadn't fired yet.
UPDATE sessions SET status = 'created' WHERE status = 'guest_ready';

-- Re-add the status CHECK without 'guest_ready' (the 0064 drop/re-add
-- pattern).
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN (
    'pending', 'queued', 'created', 'active', 'idle',
    'host_lost', 'evacuating', 'evicting', 'completed', 'failed', 'dead'
));
