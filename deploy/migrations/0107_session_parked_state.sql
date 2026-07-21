-- ADR 0101 C: `parked` becomes a real session state — the rung-2
-- paused-in-place VM (sandbox + host bound, guest RAM resident,
-- un-park ~1s). Previously spelled `status = 'evicting' AND
-- park_rung = 2`, which conflated "intentionally retained, cheap to
-- wake" with "a descent is underway": sessions read `evicting` for up
-- to the 8h hard TTL, and the scanner/op dedup interplay around that
-- compound state is the livelock class ADR 0077×0090 documented.
-- `parked_at` (0087) becomes the state's authoritative timestamp and
-- the descent operation's idempotency key.
--
-- The 0064/0077/0104 drop/re-add pattern.
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN (
    'pending', 'queued', 'created', 'active', 'parked', 'idle', 'unreachable',
    'host_lost', 'evacuating', 'evicting', 'completed', 'failed', 'dead'
));

-- The scanner sweeps parked rows (pressure / hard-TTL descent
-- candidacy) the same way it sweeps evicting rows; mirror 0050's
-- partial-index shape.
CREATE INDEX IF NOT EXISTS idx_sessions_parked
    ON sessions (id)
    WHERE status = 'parked';
