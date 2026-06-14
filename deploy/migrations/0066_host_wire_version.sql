-- Issue #229: per-host bincode wire version for the placement filter.
--
-- A Helm rollout is not atomic — coordinator pods finish in ~1 min while a
-- 40-node host DaemonSet rolls over ~20 min. During that mixed-version window
-- the coordinator must not place sessions on hosts whose bincode WIRE_VERSION
-- differs from its own, or the host's gRPC server EOFs mid-decode and the user
-- sees a misleading 400 "invalid sandbox spec". The host-agent now reports its
-- `engram_protocol::WIRE_VERSION` on every heartbeat; the scheduler excludes a
-- host whose reported version is both NONZERO and != the coordinator's, so the
-- rolling deploy drains off stale hosts instead of hard-failing on them.
--
-- Additive: defaults to 0 = "unknown / not yet reported". A 0 is tolerated by
-- the placement filter (the same soft posture as an unmeasured allocatable),
-- so a freshly-registered host that hasn't sent its first heartbeat — and any
-- pre-0066 row — is never excluded before it has had a chance to report.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS wire_version INTEGER NOT NULL DEFAULT 0;
