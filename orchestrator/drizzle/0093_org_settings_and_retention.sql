CREATE TABLE "org_setting" (
	"key" text PRIMARY KEY NOT NULL,
	"value" jsonb NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_by_user_id" text
);
--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "details_pruned_at" timestamp with time zone;--> statement-breakpoint
CREATE INDEX "automation_run_retention_idx" ON "automation_run" USING btree ("ended_at") WHERE ended_at is not null and details_pruned_at is null;