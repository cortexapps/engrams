-- ADR 0021 P1.3: sessions.harness -> sessions.mode.
--
-- Pre-0021, the per-session `harness` JSONB column carried
-- HarnessSpec — either `{"kind":"none"}` for shell-only sessions, or
-- `{"kind":"builtin","name":"<n>"}` to pick one of the harness packs
-- registered in `harness_packs`. ADR 0021 makes the harness an *image*
-- property: it's baked into the rootfs at image-bake time, and the
-- session only chooses whether the host actually drives it.
--
-- Wire that to the database by replacing the JSONB column with a
-- compact text `mode` carrying one of:
--   - `'agent'`  : drive the image's baked harness (default).
--   - `'dev_vm'` : boot the image as a pure dev VM; if the image has
--                  a baked harness, leave it resident-but-undriven.
--
-- Per ADR 0021's no-backwards-compatibility stance this is a clean
-- cutover: any pre-0021 session row's `harness` JSONB is discarded.
-- Operators drain prod (or accept that in-flight sessions die) and
-- redeploy.

ALTER TABLE sessions DROP COLUMN IF EXISTS harness;

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS mode TEXT NOT NULL DEFAULT 'agent';

-- Lock the enum at the SQL layer so a typo'd value can't slip in
-- behind the Rust enum's serde validation.
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_mode_check;
ALTER TABLE sessions
    ADD  CONSTRAINT sessions_mode_check CHECK (mode IN ('agent', 'dev_vm'));
