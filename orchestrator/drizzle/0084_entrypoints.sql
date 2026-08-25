DROP INDEX "automation_run_occurrence_unique";--> statement-breakpoint
DROP INDEX "automation_run_delivery_unique";--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "entrypoint_id" text DEFAULT 'main' NOT NULL;--> statement-breakpoint
ALTER TABLE "automation_version" ADD COLUMN "entrypoints" jsonb DEFAULT '[]'::jsonb NOT NULL;--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_occurrence_unique" ON "automation_run" USING btree ("automation_id","entrypoint_id","scheduled_for") WHERE scheduled_for is not null;--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_delivery_unique" ON "automation_run" USING btree ("automation_id","entrypoint_id","delivery_key") WHERE delivery_key is not null;