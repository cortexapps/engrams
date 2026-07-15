CREATE TABLE "papercuts" (
	"id" text PRIMARY KEY NOT NULL,
	"summary" text NOT NULL,
	"description" text NOT NULL,
	"category" text NOT NULL,
	"severity" text,
	"tags" jsonb DEFAULT '[]'::jsonb,
	"session_id" text NOT NULL,
	"task_id" text,
	"profile_id" text,
	"user_id" text,
	"fix_task_id" text,
	"archived_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "papercuts_created_at_idx" ON "papercuts" USING btree ("created_at");--> statement-breakpoint
CREATE INDEX "papercuts_archived_at_idx" ON "papercuts" USING btree ("archived_at");