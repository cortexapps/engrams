-- ADR 0116 A-D1: the host binding lease — the explicit liveness
-- contract that replaces staleness inference. Today the coordinator
-- infers host death from `last_heartbeat_at` age (with a silent 10x
-- multiplier while cordoned) and probe-strike heuristics; a planned
-- roll whose pod replacement outlives that implicit grace orphans
-- healthy sessions (2026-08-12: orphaned 44 s before the successor
-- adopted the VM, which teardown-reconcile then destroyed). The lease
-- makes the deadline WRITTEN DATA: heartbeats renew it, a planned
-- handoff extends it past the whole operation, successor adoption
-- (register) ends the handoff early. Enforcement (ADR 0116 A-D4, a
-- later phase) reads `lease_expires_at`; it never computes "how long
-- since I heard from you" with multipliers.
--
-- This is NOT the wall-clock `session_lease` that 0093 dropped: that
-- arbitrated mutual exclusion between coordinator replicas (correctly
-- replaced by the op log). This is a host<->coordinator liveness
-- contract whose deadline is host- or operator-declared fact.
--
-- All three columns are written with values bound from the coordinator
-- clock (ADR 0098 D3), never SQL now() — expiry comparisons must be
-- single-clock.
ALTER TABLE hosts
    -- The lease deadline. NULL = no renewal observed since this
    -- migration (a legacy host-agent, or a row that has not
    -- heartbeated yet): enforcement falls back to
    -- `last_heartbeat_at + LEASE_TTL`, so there is no flag day.
    -- Renewal is GREATEST(existing, now + ttl) — a predecessor's last
    -- racing heartbeat can never shrink a handoff deadline.
    ADD COLUMN lease_expires_at TIMESTAMPTZ,
    -- 'none'    = pre-migration / never renewed;
    -- 'active'  = heartbeat-sustained;
    -- 'handoff' = a planned operation (roll) declared a successor
    --             deadline; replaced by 'active' on the successor's
    --             register, or consumed by expiry.
    ADD COLUMN lease_state TEXT NOT NULL DEFAULT 'none'
        CHECK (lease_state IN ('none', 'active', 'handoff')),
    -- Host-agent generation, bumped on every register. A fence for
    -- later phases (successor adoption vs predecessor writes); never
    -- decreases.
    ADD COLUMN lease_epoch BIGINT NOT NULL DEFAULT 0;

-- The A-D4 death-path scan: "ready hosts whose lease (or legacy
-- fallback) has expired". Partial — draining/dead rows are host-
-- affirmed states and never scanned.
CREATE INDEX idx_hosts_lease_expiry
    ON hosts (lease_expires_at)
    WHERE status = 'ready';
