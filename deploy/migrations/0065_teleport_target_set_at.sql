-- Issue #214 (defense in depth): stamp the operator-pinned teleport
-- destination with the wall-clock instant it was set.
--
-- The core fix prevents the leak at its source (teleport_session now
-- removes the pin + returns 409 when the eviction pipeline no-ops). This
-- column is the second line of defense: the evac scanner ignores + clears
-- + warns on a pin older than a TTL, so any FUTURE leak path degrades to
-- default capacity-ranked placement instead of strictly hijacking the
-- session's next evacuation onto a stale (possibly full/gone) host.
--
-- NULL means "no pin / a pin set before this migration" — the scanner
-- treats a missing timestamp as not-aged (honors the pin), so existing
-- pins are unaffected by the rollout.
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS teleport_target_set_at TIMESTAMPTZ;
