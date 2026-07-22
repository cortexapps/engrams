CREATE TABLE "automation" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"enabled" boolean DEFAULT true NOT NULL,
	"trigger" jsonb NOT NULL,
	"action" jsonb NOT NULL,
	"created_by_user_id" text,
	"next_fire_at" timestamp with time zone,
	"last_fired_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	"archived_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "automation_run" (
	"id" text PRIMARY KEY NOT NULL,
	"automation_id" text NOT NULL,
	"trigger" jsonb NOT NULL,
	"rendered_prompt" text,
	"rendered_title" text,
	"task_id" text,
	"session_id" text,
	"status" text DEFAULT 'pending' NOT NULL,
	"error" text,
	"scheduled_for" timestamp with time zone,
	"lease_owner" text,
	"lease_expires_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "webhook_registration" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"verification" jsonb NOT NULL,
	"provider_hint" text,
	"created_by_user_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "webhook_sample" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"registration_id" text NOT NULL,
	"event_key" text NOT NULL,
	"payload" jsonb NOT NULL,
	"received_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "automation_run" ADD CONSTRAINT "automation_run_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_run" ADD CONSTRAINT "automation_run_task_id_task_id_fk" FOREIGN KEY ("task_id") REFERENCES "public"."task"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "webhook_sample" ADD CONSTRAINT "webhook_sample_registration_id_webhook_registration_id_fk" FOREIGN KEY ("registration_id") REFERENCES "public"."webhook_registration"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "automation_due_idx" ON "automation" USING btree ("enabled","next_fire_at");--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_occurrence_unique" ON "automation_run" USING btree ("automation_id","scheduled_for") WHERE scheduled_for is not null;--> statement-breakpoint
CREATE INDEX "automation_run_automation_created_idx" ON "automation_run" USING btree ("automation_id","created_at");--> statement-breakpoint
CREATE INDEX "automation_run_lease_idx" ON "automation_run" USING btree ("lease_expires_at");--> statement-breakpoint
CREATE INDEX "webhook_registration_provider_idx" ON "webhook_registration" USING btree ("provider_hint");--> statement-breakpoint
CREATE INDEX "webhook_sample_registration_event_idx" ON "webhook_sample" USING btree ("registration_id","event_key","received_at");