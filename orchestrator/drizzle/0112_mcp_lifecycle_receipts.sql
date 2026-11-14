CREATE TABLE "mcp_cancellation" (
	"execution_id" text PRIMARY KEY NOT NULL,
	"actor_id" text,
	"cause" text NOT NULL,
	"expected_journal_revision" integer NOT NULL,
	"journal_revision" integer NOT NULL,
	"state" text NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_cancellation_revision_check" CHECK ("mcp_cancellation"."expected_journal_revision" > 0 AND "mcp_cancellation"."journal_revision" = "mcp_cancellation"."expected_journal_revision" + 1),
	CONSTRAINT "mcp_cancellation_state_check" CHECK ("mcp_cancellation"."state" IN ('canceled','outcome_unknown'))
);
--> statement-breakpoint
ALTER TABLE "mcp_resolution" ADD COLUMN "expected_context" jsonb;--> statement-breakpoint
ALTER TABLE "mcp_cancellation" ADD CONSTRAINT "mcp_cancellation_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE cascade ON UPDATE no action;