CREATE TABLE "integration_event" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"provider" text NOT NULL,
	"connection_id" text NOT NULL,
	"event_key" text NOT NULL,
	"delivery_id" text NOT NULL,
	"payload" jsonb NOT NULL,
	"scope_value" text,
	"received_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "integration_event" ADD CONSTRAINT "integration_event_connection_id_integration_connection_id_fk" FOREIGN KEY ("connection_id") REFERENCES "public"."integration_connection"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "integration_event_delivery_unique" ON "integration_event" USING btree ("provider","connection_id","delivery_id");--> statement-breakpoint
CREATE INDEX "integration_event_conn_key_idx" ON "integration_event" USING btree ("connection_id","event_key","received_at");