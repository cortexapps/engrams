ALTER TABLE "mcp_execution" ADD COLUMN "original_arguments" text;--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD COLUMN "request_context" jsonb;