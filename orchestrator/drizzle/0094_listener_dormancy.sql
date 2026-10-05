ALTER TABLE "session_listeners" ADD COLUMN "dormant_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "session_listeners" ADD COLUMN "woken_at" timestamp with time zone;