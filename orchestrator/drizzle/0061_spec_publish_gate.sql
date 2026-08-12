CREATE TABLE "spec_publish" (
	"spec_id" uuid PRIMARY KEY NOT NULL,
	"session_id" uuid NOT NULL,
	"checkpoint_id" uuid NOT NULL,
	"artifact_id" text NOT NULL,
	"artifact_version" integer,
	"state" text NOT NULL,
	"requested_by" text,
	"requested_at" timestamp with time zone NOT NULL,
	"acknowledged_question_count" integer DEFAULT 0 NOT NULL,
	"acknowledged_question_ids" jsonb NOT NULL,
	"gap_check_run_id" uuid,
	"attempts" integer DEFAULT 0 NOT NULL,
	"next_attempt_at" timestamp with time zone NOT NULL,
	"last_error" text,
	"pinned_at" timestamp with time zone,
	"completed_at" timestamp with time zone
);
--> statement-breakpoint
ALTER TABLE "spec_publish" ADD CONSTRAINT "spec_publish_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_publish" ADD CONSTRAINT "spec_publish_requested_by_user_id_fk" FOREIGN KEY ("requested_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_publish_due_idx" ON "spec_publish" USING btree ("next_attempt_at") WHERE "spec_publish"."state" <> 'complete';
