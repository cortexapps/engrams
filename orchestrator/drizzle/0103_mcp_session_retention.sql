ALTER TABLE "mcp_admission" DROP CONSTRAINT "mcp_admission_execution_id_mcp_execution_id_fk";
--> statement-breakpoint
ALTER TABLE "mcp_approval" DROP CONSTRAINT "mcp_approval_execution_id_mcp_execution_id_fk";
--> statement-breakpoint
ALTER TABLE "mcp_execution" DROP CONSTRAINT "mcp_execution_task_id_session_id_task_session_task_id_session_id_fk";
--> statement-breakpoint
ALTER TABLE "mcp_resolution" DROP CONSTRAINT "mcp_resolution_execution_id_mcp_execution_id_fk";
--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD CONSTRAINT "mcp_admission_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_approval" ADD CONSTRAINT "mcp_approval_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD CONSTRAINT "mcp_execution_task_id_session_id_task_session_task_id_session_id_fk" FOREIGN KEY ("task_id","session_id") REFERENCES "public"."task_session"("task_id","session_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_resolution" ADD CONSTRAINT "mcp_resolution_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE cascade ON UPDATE no action;