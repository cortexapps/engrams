CREATE TABLE "spec_ticket_sync_operation" (
	"caller_spec_id" uuid NOT NULL,
	"operation" text NOT NULL,
	"idempotency_key" text NOT NULL,
	"request_hash" text NOT NULL,
	"status" text DEFAULT 'reserved' NOT NULL,
	"reserved_ticket_id" uuid,
	"reserved_external_id" text,
	"attempts" integer DEFAULT 0 NOT NULL,
	"result" jsonb,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "spec_ticket_sync_operation_caller_spec_id_operation_idempotency_key_pk" PRIMARY KEY("caller_spec_id","operation","idempotency_key")
);
--> statement-breakpoint
CREATE TABLE "spec_ticket_sync_config" (
	"spec_id" uuid PRIMARY KEY NOT NULL,
	"team_id" text,
	"team_name" text,
	"project_id" text,
	"project_name" text,
	"label_ids" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"label_names" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "spec_ticket_sync_operation" ADD CONSTRAINT "spec_ticket_sync_operation_caller_spec_id_spec_id_fk" FOREIGN KEY ("caller_spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_ticket_sync_config" ADD CONSTRAINT "spec_ticket_sync_config_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_ticket_sync_operation_ticket_idx" ON "spec_ticket_sync_operation" USING btree ("reserved_ticket_id");
