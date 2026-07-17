-- ADR 0098 D4 preamble: cross-replica dead-host eviction guard via a
-- PG leasing row, replacing dead_host.rs's `pg_try_advisory_lock` —
-- the one driver that still held a raw PgPool for its mutual
-- exclusion. Same rationale as the (since-retired) eviction_inflight
-- table (0033): a row is connection-pool friendly, operator-visible
-- (`SELECT * FROM dead_host_inflight` answers "which pod is evicting
-- which host?"), and crash-safe by TTL — a pod that dies mid-eviction
-- leaves a row that any replica may take over once `claimed_at` ages
-- past the stale window (no unlock required), instead of a session-
-- scoped lock pinned to a dead backend.
--
-- The row is deleted on every normal exit path; the stale-takeover
-- predicate in `try_acquire_dead_host_lease` is the reaper (no
-- separate sweep task — contention on a host implies a detector is
-- already looking at it).
CREATE TABLE dead_host_inflight (
    host_id    UUID PRIMARY KEY,
    -- Coordinator pod identity (HOSTNAME), for diagnostics only —
    -- release is guarded on it so a stale claimant can't delete the
    -- lease out from under the pod that took it over.
    claimed_by TEXT NOT NULL,
    -- Bound from the coordinator clock (ADR 0098 D3), not DEFAULT
    -- now() — the stale-takeover comparison must be single-clock.
    claimed_at TIMESTAMPTZ NOT NULL
);
