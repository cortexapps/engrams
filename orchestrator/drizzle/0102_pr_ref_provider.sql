DROP INDEX "pr_ref_repo_pr_number_unique";--> statement-breakpoint
ALTER TABLE "pr_ref" ADD COLUMN "provider" text DEFAULT 'github' NOT NULL;--> statement-breakpoint
CREATE UNIQUE INDEX "pr_ref_provider_repo_pr_number_unique" ON "pr_ref" USING btree ("provider","repo","pr_number");