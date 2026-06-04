-- ADR 0037: warm-captured persistent harness flag on base snapshots.
--
-- TRUE iff a warm, prompt-less `claude` harness was captured *into* this
-- base snapshot (host-side, gated by ENGRAM_WARM_HARNESS_CAPTURE). The
-- restore-fork reads it: warm_harness => late-Bind the already-running
-- warm child instead of SpawnHarness. Stamped only on enable-time base
-- captures, never on session idle-evict/drain captures.
--
-- DEFAULT FALSE so every pre-existing snapshot row reads cold and the
-- restore path automatically falls back to today's spawn. Content-keyed
-- base-snapshot reuse (enabled_images) must additionally treat warm vs
-- cold as non-equivalent, or enabling warm-capture silently reuses a
-- cold snapshot.
ALTER TABLE snapshots
    ADD COLUMN warm_harness BOOLEAN NOT NULL DEFAULT FALSE;
