CREATE TABLE "pending_tool_calls" (
	"session_id" text NOT NULL,
	"tool_call_id" text NOT NULL,
	"tool_name" text NOT NULL,
	"handling" text NOT NULL,
	"requested_at" timestamp with time zone NOT NULL,
	"submitted_at" timestamp with time zone,
	"completed_at" timestamp with time zone
);
--> statement-breakpoint
CREATE UNIQUE INDEX "pending_tool_calls_tool_call_id_unique" ON "pending_tool_calls" USING btree ("tool_call_id");--> statement-breakpoint
CREATE INDEX "pending_tool_calls_session_idx" ON "pending_tool_calls" USING btree ("session_id");