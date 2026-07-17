CREATE TABLE "pr_ref" (
	"id" text PRIMARY KEY NOT NULL,
	"repo" text NOT NULL,
	"pr_number" integer NOT NULL,
	"authoring_task_id" text,
	"session_id" text NOT NULL,
	"title" text NOT NULL,
	"url" text NOT NULL,
	"head_branch" text NOT NULL,
	"base_branch" text NOT NULL,
	"observed_at" timestamp with time zone NOT NULL
);
--> statement-breakpoint
ALTER TABLE "pr_ref" ADD CONSTRAINT "pr_ref_authoring_task_id_task_id_fk" FOREIGN KEY ("authoring_task_id") REFERENCES "public"."task"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "pr_ref_repo_pr_number_unique" ON "pr_ref" USING btree ("repo","pr_number");--> statement-breakpoint
CREATE INDEX "pr_ref_authoring_task_idx" ON "pr_ref" USING btree ("authoring_task_id");--> statement-breakpoint
CREATE INDEX "pr_ref_session_idx" ON "pr_ref" USING btree ("session_id");