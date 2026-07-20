CREATE TABLE "review" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"repo" text NOT NULL,
	"pr_number" integer NOT NULL,
	"task_id" text NOT NULL,
	"head_sha" text NOT NULL,
	"base_sha" text NOT NULL,
	"trigger" text NOT NULL,
	"status" text DEFAULT 'queued' NOT NULL,
	"github_review_id" text,
	"summary_md" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "review_finding" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"review_id" uuid NOT NULL,
	"path" text NOT NULL,
	"start_line" integer,
	"end_line" integer,
	"side" text,
	"category" text NOT NULL,
	"severity" text NOT NULL,
	"confidence" text NOT NULL,
	"title" text NOT NULL,
	"body_md" text NOT NULL,
	"suggested_fix" text,
	"evidence" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"state" text DEFAULT 'candidate' NOT NULL,
	"verdict_reason" text,
	"github_thread_id" text,
	"resolution" text,
	"session_id" text NOT NULL,
	"tool_call_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "review_verdict" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"finding_id" uuid NOT NULL,
	"verdict" text NOT NULL,
	"confidence" text NOT NULL,
	"reasoning" text NOT NULL,
	"session_id" text NOT NULL,
	"tool_call_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "review" ADD CONSTRAINT "review_task_id_task_id_fk" FOREIGN KEY ("task_id") REFERENCES "public"."task"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "review_finding" ADD CONSTRAINT "review_finding_review_id_review_id_fk" FOREIGN KEY ("review_id") REFERENCES "public"."review"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "review_verdict" ADD CONSTRAINT "review_verdict_finding_id_review_finding_id_fk" FOREIGN KEY ("finding_id") REFERENCES "public"."review_finding"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "review_repo_pr_number_idx" ON "review" USING btree ("repo","pr_number");--> statement-breakpoint
CREATE INDEX "review_task_idx" ON "review" USING btree ("task_id");--> statement-breakpoint
CREATE UNIQUE INDEX "review_finding_session_tool_call_unique" ON "review_finding" USING btree ("session_id","tool_call_id");--> statement-breakpoint
CREATE INDEX "review_finding_review_idx" ON "review_finding" USING btree ("review_id");--> statement-breakpoint
CREATE UNIQUE INDEX "review_verdict_session_tool_call_unique" ON "review_verdict" USING btree ("session_id","tool_call_id");--> statement-breakpoint
CREATE UNIQUE INDEX "review_verdict_finding_unique" ON "review_verdict" USING btree ("finding_id");