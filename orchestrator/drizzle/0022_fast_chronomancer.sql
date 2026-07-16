ALTER TABLE "papercuts" ADD COLUMN "tool_call_id" text;--> statement-breakpoint
CREATE UNIQUE INDEX "papercuts_session_tool_call_unique" ON "papercuts" USING btree ("session_id","tool_call_id");--> statement-breakpoint
ALTER TABLE "papercuts" DROP COLUMN "fix_task_id";