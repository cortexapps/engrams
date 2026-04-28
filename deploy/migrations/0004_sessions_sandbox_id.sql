-- Phase 3 follow-up: persist `sandbox_id` on sessions so the
-- coordinator can rebuild its in-memory routing maps after a
-- restart. Pre-restart the in-memory `SandboxRegistry`
-- (`session_id -> sandbox_id`) and `HostRegistry.sandbox_owner`
-- (`sandbox_id -> host_id`) hold this mapping; with this column
-- both can be repopulated from `SELECT id, sandbox_id, host_id
-- FROM sessions WHERE status NOT IN ('completed','failed')` at
-- coordinator startup.
--
-- Nullable because (a) the row is created before the sandbox
-- exists (create_session inserts the row, then asks the host
-- to spin up the sandbox), and (b) terminal sessions whose
-- sandboxes have been destroyed don't have one anymore.

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS sandbox_id UUID;
