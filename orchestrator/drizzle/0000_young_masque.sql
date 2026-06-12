CREATE TABLE "task" (
	"id" text PRIMARY KEY NOT NULL,
	"type" text NOT NULL,
	"title" text,
	"status" text DEFAULT 'open' NOT NULL,
	"created_by_user_id" text,
	"source" jsonb,
	"workflow_run_id" text,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "task_session" (
	"task_id" text NOT NULL,
	"session_id" text NOT NULL,
	"role" text,
	"created_at" timestamp DEFAULT now() NOT NULL,
	CONSTRAINT "task_session_task_id_session_id_pk" PRIMARY KEY("task_id","session_id")
);
--> statement-breakpoint
ALTER TABLE "task_session" ADD CONSTRAINT "task_session_task_id_task_id_fk" FOREIGN KEY ("task_id") REFERENCES "public"."task"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "task_session_session_idx" ON "task_session" USING btree ("session_id");