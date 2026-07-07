-- ADR 0073 / epic #542: binding generation for the session->sandbox->harness
-- attach. Bumped atomically in the same statement as every sandbox_id
-- (re)assignment; carried in the attach token and validated by the host
-- hub against its durable binding record. Distinct from the (future,
-- epic #543) op-fencing epoch — see ADR 0073 "Two epochs".
--
-- NOTE: numbered 0083 against the 2026-07-05 high-water mark (0077 on main,
-- 0078-0082 held by open PRs #561/#563/#564/#565/#566). Re-verify at land.
ALTER TABLE sessions ADD COLUMN binding_epoch BIGINT NOT NULL DEFAULT 0;
