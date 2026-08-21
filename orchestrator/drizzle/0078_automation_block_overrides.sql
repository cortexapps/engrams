ALTER TABLE "automation" ADD COLUMN "block_overrides" jsonb DEFAULT '{}'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "dry_run" boolean DEFAULT false NOT NULL;
