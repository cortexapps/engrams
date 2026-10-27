CREATE TABLE "automation_session_relay" (
	"session_id" text PRIMARY KEY NOT NULL,
	"automation_id" text NOT NULL,
	"provider" text NOT NULL,
	"target" jsonb NOT NULL,
	"state" jsonb NOT NULL,
	"version" bigint DEFAULT 1 NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "automation_session_relay" ADD CONSTRAINT "automation_session_relay_session_id_automation_session_session_id_fk" FOREIGN KEY ("session_id") REFERENCES "public"."automation_session"("session_id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_session_relay" ADD CONSTRAINT "automation_session_relay_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_session" DROP COLUMN "relay";