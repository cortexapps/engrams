DROP INDEX "review_repo_pr_number_idx";--> statement-breakpoint
ALTER TABLE "review" ALTER COLUMN "target_id" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "repo";--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "pr_number";--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "pr_title";--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "pr_author";--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "pr_state";