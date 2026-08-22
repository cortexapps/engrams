-- Repair for 0080 (ADR 0119 phase 4.4). 0080 shipped with a journal `when`
-- LOWER than 0081's, and 0081 merged first. The drizzle migrator applies an
-- entry only when its `when` is greater than the last applied migration's, so
-- every database that applied 0081 before 0080 existed skipped 0080 for good
-- (prod did). This entry carries a `when` above 0081 and is idempotent, so a
-- fresh database (which ran 0080) and a skipped database both end with the
-- column.
ALTER TABLE "review_enrollment" ADD COLUMN IF NOT EXISTS "engine" text DEFAULT 'legacy' NOT NULL;
