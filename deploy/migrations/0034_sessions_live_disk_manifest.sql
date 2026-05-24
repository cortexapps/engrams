-- ADR 0016 Phase B: per-session live disk manifest pointer for the
-- continuous-flush scheduler. The host's FlushScheduler publishes the
-- latest chunked-disk manifest version every 30s (or on dirty-bytes
-- threshold) via `POST /internal/live-manifest`; the coord-side
-- `MetadataStore::update_live_disk_manifest` writes these columns
-- inside the same TX that bumps `chunk_generation` for Phase C's
-- mid-sweep barrier.
--
-- Column shape mirrors `snapshots.disk_manifest_*` (migration 0018) —
-- separate UUID + BIGINT for the `ManifestRef`'s two-tuple. The
-- coord-side resolver `effective_resume_disk_manifest` (commit 6)
-- picks max(snapshots.disk_manifest, sessions.live_disk_manifest)
-- on resume; without that pick, the first resume after Phase B's
-- continuous flush is enabled silently rolls back to the snapshot's
-- stale manifest, throwing away every flush since.
--
-- Migration number bump: the ADR originally spec'd `0033_` for these
-- columns but A.1.5c took that slot for `eviction_inflight`
-- (migration 0033, 2026-05-24). Phase B takes 0034.

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS live_disk_manifest_id      UUID,
    ADD COLUMN IF NOT EXISTS live_disk_manifest_version BIGINT,
    -- Wall-clock of the last successful publish. Diagnostic / debugging
    -- only — the truth is the (id, version) pair. NULL until first
    -- publish; cleared together with the manifest pair when the session
    -- transitions to Idle (see `MetadataStore::transition_session`).
    ADD COLUMN IF NOT EXISTS live_disk_manifest_at      TIMESTAMPTZ;

-- Both-or-neither: an id without a version (or vice-versa) is a bug.
-- `live_disk_manifest_at` is a hint, not part of the manifest identity,
-- so it's allowed to be NULL even when the pair is set (covers backfill
-- and the rare publish that lands before NOW() resolves on the writer).
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_live_disk_manifest_both_or_neither;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_live_disk_manifest_both_or_neither CHECK (
        (live_disk_manifest_id IS NULL) = (live_disk_manifest_version IS NULL)
    );

-- Phase C's pin-set will SELECT live_disk_manifest_id for every Active
-- session to compute "this manifest lineage is live, do not GC its
-- chunks." Partial-index it — terminal/Idle rows have NULL and are
-- naturally excluded.
CREATE INDEX IF NOT EXISTS idx_sessions_live_disk_manifest
    ON sessions (live_disk_manifest_id)
    WHERE live_disk_manifest_id IS NOT NULL;

-- Phase C: one-row barrier counter. The chunk GC reads `generation`
-- before listing the pin set, again after listing, and restarts the
-- sweep if it ticked — closing the race where a flush published a
-- new manifest mid-sweep and our pin set is stale. Phase B bumps
-- this in the same TX that writes a successful
-- `update_live_disk_manifest`; commit 4 also bumps on idle-transition
-- cleanup. The table is single-row by construction (CHECK on `id`).
CREATE TABLE IF NOT EXISTS chunk_generation (
    -- Single-row guard. BOOL DEFAULT TRUE + CHECK keeps exactly one
    -- row addressable; concurrent UPDATEs serialize on the row lock.
    id         BOOLEAN PRIMARY KEY DEFAULT TRUE,
    generation BIGINT NOT NULL    DEFAULT 0,
    CHECK (id)
);

-- Seed the row. Idempotent so a re-run of the migration on a partial
-- state doesn't fail.
INSERT INTO chunk_generation (id, generation) VALUES (TRUE, 0)
    ON CONFLICT (id) DO NOTHING;
