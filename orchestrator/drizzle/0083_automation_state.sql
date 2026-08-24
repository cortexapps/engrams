CREATE TABLE "automation_state" (
	"automation_id" text NOT NULL,
	"key" text NOT NULL,
	"value" jsonb NOT NULL,
	"version" bigint DEFAULT 1 NOT NULL,
	"writer" text DEFAULT '' NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "automation_state_automation_id_key_pk" PRIMARY KEY("automation_id","key")
);
--> statement-breakpoint
ALTER TABLE "automation_state" ADD CONSTRAINT "automation_state_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;