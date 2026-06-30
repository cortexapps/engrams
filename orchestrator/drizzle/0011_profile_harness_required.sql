-- ADR 0063: a profile always names a concrete harness (no "inherit deployment
-- default"). Backfill existing rows to the built-in `claude` BEFORE the NOT NULL.
UPDATE "profile" SET "harness" = 'claude' WHERE "harness" IS NULL;--> statement-breakpoint
ALTER TABLE "profile" ALTER COLUMN "harness" SET NOT NULL;
