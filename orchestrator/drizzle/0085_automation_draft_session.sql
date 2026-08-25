ALTER TABLE "automation" ADD COLUMN "draft_session_id" text;--> statement-breakpoint
CREATE INDEX "automation_draft_session_idx" ON "automation" USING btree ("draft_session_id");