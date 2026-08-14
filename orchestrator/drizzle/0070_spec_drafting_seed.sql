CREATE TABLE "spec_drafting_seed" (
	"spec_id" uuid PRIMARY KEY NOT NULL,
	"session_id" uuid NOT NULL,
	"prompt_id" text NOT NULL,
	"text" text NOT NULL,
	"state" text NOT NULL,
	"attempts" integer DEFAULT 0 NOT NULL,
	"next_attempt_at" timestamp with time zone NOT NULL,
	"last_error" text,
	"requested_at" timestamp with time zone NOT NULL,
	"delivered_at" timestamp with time zone,
	CONSTRAINT "spec_drafting_seed_prompt_id_unique" UNIQUE("prompt_id"),
	CONSTRAINT "spec_drafting_seed_state_check" CHECK ("state" IN ('pending', 'delivered'))
);
--> statement-breakpoint
ALTER TABLE "spec_drafting_seed" ADD CONSTRAINT "spec_drafting_seed_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;
--> statement-breakpoint
CREATE INDEX "spec_drafting_seed_due_idx" ON "spec_drafting_seed" USING btree ("next_attempt_at") WHERE "spec_drafting_seed"."state" = 'pending';
