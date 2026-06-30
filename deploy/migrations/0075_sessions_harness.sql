-- ADR 0062: per-session harness selection. The harness is chosen per session
-- (not baked into the image), so the choice must be persisted on the session so
-- the queue scanner (boots a `queued` row with no live request) and resume can
-- reconstruct which harness to mount on `dyn_0` + exec.
--
-- NULL for a dev-VM session (no harness) or a legacy row. (Migration 0039 once
-- replaced an old `sessions.harness` column with `mode`; this re-introduces a
-- harness selection under the new catalog model — distinct meaning: a catalog
-- name, not a baked-image property.)

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS harness TEXT;
