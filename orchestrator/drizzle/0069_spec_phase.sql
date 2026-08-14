ALTER TABLE "spec" RENAME COLUMN "lifecycle" TO "phase";--> statement-breakpoint
UPDATE "spec" SET "phase" = 'drafting' WHERE "phase" = 'draft';--> statement-breakpoint
ALTER TABLE "spec" ADD CONSTRAINT "spec_phase_check" CHECK ("spec"."phase" IN ('ideation', 'drafting', 'published'));
