ALTER TABLE "review" ADD COLUMN "pr_title" text;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "pr_author" text;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "head_branch" text;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "base_branch" text;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "pr_state" text;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "additions" integer;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "deletions" integer;--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "changed_files" integer;