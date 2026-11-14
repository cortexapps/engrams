CREATE TABLE "mcp_discovery_admission" (
	"request_id" text PRIMARY KEY NOT NULL,
	"intent_id" text NOT NULL,
	"deployment_id" text NOT NULL,
	"connection_id" text NOT NULL,
	"actor_id" text NOT NULL,
	"config_revision" integer NOT NULL,
	"auth_generation" integer NOT NULL,
	"credential_identity" text NOT NULL,
	"identity" jsonb NOT NULL,
	"destination" text NOT NULL,
	"request_digest" text NOT NULL,
	"status_digest" text NOT NULL,
	"state" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_discovery_admission_state_check" CHECK ("mcp_discovery_admission"."state" IN ('reserved','dispatched','finished')),
	CONSTRAINT "mcp_discovery_admission_expiry_check" CHECK ("mcp_discovery_admission"."expires_at" = "mcp_discovery_admission"."created_at" + interval '30 seconds')
);
--> statement-breakpoint
ALTER TABLE "mcp_discovery_admission" ADD CONSTRAINT "mcp_discovery_admission_deployment_id_mcp_admission_scope_id_fk" FOREIGN KEY ("deployment_id") REFERENCES "public"."mcp_admission_scope"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_discovery_admission" ADD CONSTRAINT "mcp_discovery_admission_connection_id_integration_connection_id_fk" FOREIGN KEY ("connection_id") REFERENCES "public"."integration_connection"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_discovery_admission_intent_idx" ON "mcp_discovery_admission" USING btree ("intent_id");--> statement-breakpoint
CREATE INDEX "mcp_discovery_admission_deployment_idx" ON "mcp_discovery_admission" USING btree ("deployment_id","state","expires_at");--> statement-breakpoint
CREATE INDEX "mcp_discovery_admission_connection_idx" ON "mcp_discovery_admission" USING btree ("connection_id","state","expires_at");--> statement-breakpoint
CREATE FUNCTION mcp_discovery_admission_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'state') IS DISTINCT FROM (to_jsonb(OLD) - 'state') THEN
    RAISE EXCEPTION 'MCP discovery identity is immutable';
  END IF;
  IF NEW.state <> OLD.state AND NOT (
    (OLD.state = 'reserved' AND NEW.state IN ('dispatched', 'finished')) OR
    (OLD.state = 'dispatched' AND NEW.state = 'finished')
  ) THEN
    RAISE EXCEPTION 'invalid MCP discovery transition';
  END IF;
  RETURN NEW;
END;
$$;
--> statement-breakpoint
CREATE TRIGGER mcp_discovery_admission_transition BEFORE UPDATE ON mcp_discovery_admission
  FOR EACH ROW EXECUTE FUNCTION mcp_discovery_admission_transition();
