CREATE TABLE "review_session" (
	"session_id" text PRIMARY KEY NOT NULL,
	"review_workflow_id" text NOT NULL,
	"role" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
