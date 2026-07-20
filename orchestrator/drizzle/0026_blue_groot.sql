CREATE TABLE "review_enrollment" (
	"repo" text PRIMARY KEY NOT NULL,
	"trigger_mode" text DEFAULT 'manual' NOT NULL,
	"autofix" text DEFAULT 'off' NOT NULL,
	"profile_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "review_enrollment" ADD CONSTRAINT "review_enrollment_profile_id_profile_id_fk" FOREIGN KEY ("profile_id") REFERENCES "public"."profile"("id") ON DELETE no action ON UPDATE no action;