CREATE TABLE "automation_instance" (
	"id" text PRIMARY KEY NOT NULL,
	"automation_id" text NOT NULL,
	"key" text NOT NULL,
	"status" text DEFAULT 'open' NOT NULL,
	"inputs" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"opened_by" text DEFAULT '' NOT NULL,
	"opened_at" timestamp with time zone DEFAULT now() NOT NULL,
	"closed_at" timestamp with time zone,
	"close_reason" text
);
--> statement-breakpoint
CREATE TABLE "automation_instance_handle" (
	"automation_id" text NOT NULL,
	"handle" text NOT NULL,
	"instance_id" text NOT NULL,
	"written_by" text DEFAULT '' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "automation_instance_handle_automation_id_handle_pk" PRIMARY KEY("automation_id","handle")
);
--> statement-breakpoint
DROP INDEX "automation_run_occurrence_unique";--> statement-breakpoint
DROP INDEX "automation_run_delivery_unique";--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "instance_id" text DEFAULT '' NOT NULL;--> statement-breakpoint
ALTER TABLE "automation_instance" ADD CONSTRAINT "automation_instance_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_instance_handle" ADD CONSTRAINT "automation_instance_handle_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_instance_handle" ADD CONSTRAINT "automation_instance_handle_instance_id_automation_instance_id_fk" FOREIGN KEY ("instance_id") REFERENCES "public"."automation_instance"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "automation_instance_open_key_unique" ON "automation_instance" USING btree ("automation_id","key") WHERE status = 'open';--> statement-breakpoint
CREATE INDEX "automation_instance_automation_idx" ON "automation_instance" USING btree ("automation_id","status","opened_at");--> statement-breakpoint
CREATE INDEX "automation_instance_handle_instance_idx" ON "automation_instance_handle" USING btree ("instance_id");--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_occurrence_unique" ON "automation_run" USING btree ("automation_id","entrypoint_id","instance_id","scheduled_for") WHERE scheduled_for is not null;--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_delivery_unique" ON "automation_run" USING btree ("automation_id","entrypoint_id","delivery_key") WHERE delivery_key is not null and scheduled_for is null;