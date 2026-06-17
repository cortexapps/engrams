CREATE TABLE "profile" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"icon" text DEFAULT 'Bot' NOT NULL,
	"image_id" text NOT NULL,
	"include_user_tokens" boolean DEFAULT false NOT NULL,
	"env_vars" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL,
	"deleted_at" timestamp
);
--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "profile_id" text;--> statement-breakpoint
ALTER TABLE "task_session" ADD CONSTRAINT "task_session_profile_id_profile_id_fk" FOREIGN KEY ("profile_id") REFERENCES "public"."profile"("id") ON DELETE no action ON UPDATE no action;