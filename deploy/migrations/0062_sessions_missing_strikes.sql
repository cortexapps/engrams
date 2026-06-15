-- ADR 0047: the reconciler's missing-sandbox strike counter moves onto
-- the session row. Per-pod in-memory counters corrupt under N replicas:
-- a host's heartbeats round-robin across pods, so "present" resets land
-- on one pod while strikes accumulate on another — a healthy session
-- could be flipped HostLost without ever being missing N CONSECUTIVE
-- ticks. One shared counter restores the consecutive semantics.
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS missing_strikes INTEGER NOT NULL DEFAULT 0;
