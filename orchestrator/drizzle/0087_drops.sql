CREATE TABLE "automation_drop" (
	"id" bigserial PRIMARY KEY NOT NULL,
	"automation_id" text NOT NULL,
	"entrypoint_id" text DEFAULT 'main' NOT NULL,
	"event_key" text DEFAULT '' NOT NULL,
	"reason" text NOT NULL,
	"detail" text DEFAULT '' NOT NULL,
	"dropped_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "automation_drop" ADD CONSTRAINT "automation_drop_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "automation_drop_automation_idx" ON "automation_drop" USING btree ("automation_id","id");