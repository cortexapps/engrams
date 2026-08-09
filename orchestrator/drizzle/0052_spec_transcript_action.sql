CREATE TABLE "spec_transcript_action" (
	"id" text PRIMARY KEY NOT NULL,
	"spec_id" uuid NOT NULL,
	"section_id" text NOT NULL,
	"request_fingerprint" text NOT NULL,
	"chip" jsonb NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	"delivered_at" timestamp with time zone
);
--> statement-breakpoint
ALTER TABLE "spec_open_question" ADD COLUMN "request_fingerprint" text NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_transcript_action" ADD CONSTRAINT "spec_transcript_action_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_transcript_action_pending_idx" ON "spec_transcript_action" USING btree ("created_at","id") WHERE "spec_transcript_action"."delivered_at" IS NULL;