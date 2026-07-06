-- ADR 0073 phase 4: shell keep-alive as a PG column. The coordinator's
-- WS shell bridge stamps/renews this on its keepalive; the idle
-- detector skips sessions with a pin in the future. Expiry is implicit
-- (a dead bridge stops stamping), which deletes the host-side shell
-- refcount + renew RPCs + stale sweep (issue #219 class) outright.
--
-- NOTE: numbered 0085 against the 2026-07-05 high-water mark (0083 +
-- 0084 in this same PR). Re-verify at land time.
ALTER TABLE sessions ADD COLUMN shell_pinned_until timestamptz;
