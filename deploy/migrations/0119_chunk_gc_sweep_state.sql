-- Chunk-GC sweep state: the shard cursor and the single-writer lease.
--
-- The chunk sweep used to try to mark the ENTIRE key space in one tick.
-- That is not a bounded amount of work — it grows with accumulated
-- garbage, and in 2026-08 it outgrew a tick: the mark pass ran for 46
-- hours without completing, so the promote pass never ran and nothing
-- was ever deleted.
--
-- The sweep now walks the space in hash-prefix shards
-- (`chunks/sha256/<2 hex>/`) under a wall-clock budget, resuming from a
-- persisted cursor. Slicing is sound because each slice is still
-- classified against the COMPLETE pin set: only the key space is
-- partitioned, never the pin set.
--
-- Cursor and lease share ONE singleton row on purpose. A tick claims
-- the lease and reads the cursor in a single round trip, then releases
-- the lease and persists the cursor in another.
CREATE TABLE chunk_gc_sweep_state (
    -- Singleton guard, same shape as `chunk_generation`.
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),

    -- Next hash-prefix shard to walk, 0..255. Wrapping past 255 back to
    -- 0 is what completes a full cycle of the key space.
    next_shard INTEGER NOT NULL DEFAULT 0
        CHECK (next_shard >= 0 AND next_shard < 256),

    -- Single-writer lease, same shape as `dead_host_inflight` (0106):
    -- claimant identity plus a claim timestamp, with staleness decided
    -- by the reader against a `stale_after` window rather than stored as
    -- an expiry. No separate reaper — takeover IS the reaper.
    --
    -- Both coordinator replicas run the sweep loop. That was merely
    -- wasteful while every sweep re-walked the whole space, but with a
    -- cursor it becomes a CORRECTNESS problem: two pods advancing the
    -- same cursor would skip shards, and those shards would silently
    -- never be scanned. A PG leasing row is the house pattern for
    -- cross-pod one-at-a-time (AGENTS.md), not an advisory lock.
    --
    -- NULL claimed_by = free. A claim older than the caller's
    -- `stale_after` is taken over, so a pod that dies mid-sweep does not
    -- wedge GC until someone notices.
    claimed_by TEXT,
    -- Bound from the coordinator clock (ADR 0098 D3), never SQL now():
    -- the staleness comparison must be single-clock.
    claimed_at TIMESTAMPTZ,

    -- Last cursor advance, for operator visibility only.
    updated_at TIMESTAMPTZ
);

-- Seed the singleton so claiming is a plain guarded UPDATE and never has
-- to handle a missing row.
INSERT INTO chunk_gc_sweep_state (id, next_shard) VALUES (TRUE, 0);
