ALTER TABLE "mcp_admission" DROP CONSTRAINT "mcp_admission_state_check";--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD COLUMN "phase" text DEFAULT 'execute' NOT NULL;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD COLUMN "schema_queue_expires_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD COLUMN "execute_queue_expires_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD COLUMN "schema_digest" text;--> statement-breakpoint
ALTER TABLE "profile" ADD COLUMN "mcp_grant_publication" jsonb;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD CONSTRAINT "mcp_admission_state_check" CHECK ("mcp_admission"."state" IN ('queued','admitted','released'));