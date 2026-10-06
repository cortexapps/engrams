-- Delivery and settlement keys are scoped to a session. Keep key lookup
-- bounded as the transcript grows, and enforce uniqueness in the database.
CREATE UNIQUE INDEX session_events_idempotency_key
    ON session_events (session_id, (payload->>'idempotency_key'))
    WHERE payload->>'idempotency_key' IS NOT NULL;
