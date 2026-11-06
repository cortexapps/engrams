-- The quarantined-survivor eviction flavour is retired. A queued evict op
-- it enqueued would now run as a plain eviction that is no longer pinned
-- to the sandbox it was minted for, so the roll cancels them.
UPDATE session_ops
SET state = 'cancelled',
    finished_at = now(),
    error = 'quarantined-survivor eviction retired'
WHERE kind = 'evict'
  AND state = 'queued'
  AND (idempotency_key LIKE 'adr0090-quarantine:%'
       OR (payload->>'quarantine')::boolean IS TRUE);
