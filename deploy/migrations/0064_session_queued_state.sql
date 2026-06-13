-- ADR 0048: queued sessions. A create (or resume) that finds no host
-- capacity parks here instead of 503-ing; the queue scanner re-attempts
-- placement FIFO until it fits or times out.
--
-- Re-add the status CHECK with 'queued' (the 0050 drop/re-add pattern).
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN (
    'pending', 'queued', 'created', 'guest_ready', 'active', 'idle',
    'host_lost', 'evacuating', 'evicting', 'completed', 'failed', 'dead'
));

-- queued_at: FIFO ordering + the timeout clock. queue_origin: a create
-- waiting to boot vs a resume waiting to rehydrate (different dequeue +
-- timeout targets). queue_prompt: a create's initial prompt, stashed so
-- the scanner can reconstruct the boot off the durable row (secrets ride
-- the sealed session_secrets table, never this column).
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS queued_at    TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS queue_origin TEXT
        CHECK (queue_origin IS NULL OR queue_origin IN ('create', 'resume')),
    ADD COLUMN IF NOT EXISTS queue_prompt TEXT;

-- The scanner sweeps queued rows oldest-first; a partial index keeps that
-- O(queue depth), not O(all sessions).
CREATE INDEX IF NOT EXISTS idx_sessions_queued
    ON sessions (queued_at)
    WHERE status = 'queued';
