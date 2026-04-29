-- Phase 4 demo cleanup: drop the unused `last_harness_event_at`
-- column and its partial index.
--
-- Migration 0006 added the column expecting the host-agent's
-- HarnessHub to flush per-sandbox event timestamps to it on a
-- ~5s cadence (so the idle evictor could query "active sessions
-- past TTL" via SQL). The actual eviction path uses
-- `HarnessHub::idle_sandboxes(ttl)` which scans an in-memory
-- `last_event_at` map and returns `(SessionId, SandboxId)` pairs
-- directly — no SQL involved. The column was only ever read, never
-- written, so it shipped as permanently NULL.
--
-- Dropping it now keeps the schema honest. If a future multi-host
-- migration needs persisted timestamps to coordinate eviction
-- across hosts, the column can be re-added with the same shape.

DROP INDEX IF EXISTS sessions_idle_eviction_idx;
ALTER TABLE sessions DROP COLUMN IF EXISTS last_harness_event_at;
