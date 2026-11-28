CREATE TABLE "mcp_admission" (
	"execution_id" text PRIMARY KEY NOT NULL,
	"deployment_id" text NOT NULL,
	"connection_id" text NOT NULL,
	"session_id" text NOT NULL,
	"state" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_admission_state_check" CHECK ("mcp_admission"."state" IN ('queued','admitted'))
);
--> statement-breakpoint
CREATE TABLE "mcp_admission_scope" (
	"id" text PRIMARY KEY NOT NULL
);
--> statement-breakpoint
CREATE TABLE "mcp_approval" (
	"execution_id" text PRIMARY KEY NOT NULL,
	"deployment_id" text NOT NULL,
	"session_id" text NOT NULL,
	"request_digest" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL,
	"decision" text NOT NULL,
	"actor_id" text,
	"decided_at" timestamp with time zone,
	"consumed_at" timestamp with time zone,
	CONSTRAINT "mcp_approval_decision_check" CHECK ("mcp_approval"."decision" IN ('pending','approved','denied','expired')),
	CONSTRAINT "mcp_approval_consumed_check" CHECK ("mcp_approval"."consumed_at" IS NULL OR ("mcp_approval"."decision" = 'approved' AND "mcp_approval"."actor_id" IS NOT NULL))
);
--> statement-breakpoint
CREATE TABLE "mcp_catalog" (
	"id" text PRIMARY KEY NOT NULL,
	"connection_id" text NOT NULL,
	"kind" text NOT NULL,
	"revision" integer NOT NULL,
	"config_revision" integer NOT NULL,
	"auth_generation" integer NOT NULL,
	"credential_identity" text NOT NULL,
	"tools" jsonb NOT NULL,
	"fingerprint" text NOT NULL,
	"actor_id" text NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_catalog_kind_check" CHECK ("mcp_catalog"."kind" IN ('draft','published') AND (("mcp_catalog"."kind" = 'draft' AND "mcp_catalog"."revision" = 0) OR ("mcp_catalog"."kind" = 'published' AND "mcp_catalog"."revision" > 0)))
);
--> statement-breakpoint
CREATE TABLE "mcp_execution" (
	"id" text PRIMARY KEY NOT NULL,
	"task_id" text NOT NULL,
	"session_id" text NOT NULL,
	"run_id" text NOT NULL,
	"tool_call_id" text NOT NULL,
	"connection_id" text NOT NULL,
	"catalog_id" text NOT NULL,
	"principal_id" text NOT NULL,
	"binding_hash" text NOT NULL,
	"request_digest" text NOT NULL,
	"argument_hash" text NOT NULL,
	"upstream_name" text NOT NULL,
	"config_revision" integer NOT NULL,
	"auth_generation" integer NOT NULL,
	"classification" text NOT NULL,
	"state" text NOT NULL,
	"version" integer DEFAULT 1 NOT NULL,
	"boot_id" text,
	"intent_id" text,
	"intent_expires_at" timestamp with time zone,
	"deadline" timestamp with time zone NOT NULL,
	"result" jsonb,
	"result_unavailable" boolean DEFAULT false NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	"updated_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_execution_state_check" CHECK ("mcp_execution"."state" IN ('waiting_approval','authorized','dispatching','succeeded','failed','canceled','outcome_unknown')),
	CONSTRAINT "mcp_execution_classification_check" CHECK ("mcp_execution"."classification" IN ('read','write')),
	CONSTRAINT "mcp_execution_dispatch_check" CHECK ("mcp_execution"."state" NOT IN ('dispatching','outcome_unknown','succeeded') OR ("mcp_execution"."boot_id" IS NOT NULL AND "mcp_execution"."intent_id" IS NOT NULL AND "mcp_execution"."intent_expires_at" IS NOT NULL))
);
--> statement-breakpoint
CREATE TABLE "mcp_resolution" (
	"execution_id" text PRIMARY KEY NOT NULL,
	"actor_id" text NOT NULL,
	"outcome" text NOT NULL,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "mcp_resolution_outcome_check" CHECK ("mcp_resolution"."outcome" IN ('succeeded','failed','acknowledged'))
);
--> statement-breakpoint
ALTER TABLE "integration_connection" ADD COLUMN "config_revision" integer DEFAULT 1 NOT NULL;--> statement-breakpoint
ALTER TABLE "integration_connection" ADD COLUMN "auth_generation" integer DEFAULT 1 NOT NULL;--> statement-breakpoint
ALTER TABLE "integration_connection" ADD COLUMN "credential_identity" text DEFAULT '' NOT NULL;--> statement-breakpoint
ALTER TABLE "integration_connection" ADD COLUMN "catalog_revision" integer DEFAULT 0 NOT NULL;--> statement-breakpoint
ALTER TABLE "integration_connection" ADD COLUMN "revoked" boolean DEFAULT false NOT NULL;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD CONSTRAINT "mcp_admission_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD CONSTRAINT "mcp_admission_deployment_id_mcp_admission_scope_id_fk" FOREIGN KEY ("deployment_id") REFERENCES "public"."mcp_admission_scope"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_admission" ADD CONSTRAINT "mcp_admission_connection_id_integration_connection_id_fk" FOREIGN KEY ("connection_id") REFERENCES "public"."integration_connection"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_approval" ADD CONSTRAINT "mcp_approval_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_catalog" ADD CONSTRAINT "mcp_catalog_connection_id_integration_connection_id_fk" FOREIGN KEY ("connection_id") REFERENCES "public"."integration_connection"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD CONSTRAINT "mcp_execution_connection_id_integration_connection_id_fk" FOREIGN KEY ("connection_id") REFERENCES "public"."integration_connection"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD CONSTRAINT "mcp_execution_catalog_id_mcp_catalog_id_fk" FOREIGN KEY ("catalog_id") REFERENCES "public"."mcp_catalog"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD CONSTRAINT "mcp_execution_task_id_session_id_task_session_task_id_session_id_fk" FOREIGN KEY ("task_id","session_id") REFERENCES "public"."task_session"("task_id","session_id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "mcp_resolution" ADD CONSTRAINT "mcp_resolution_execution_id_mcp_execution_id_fk" FOREIGN KEY ("execution_id") REFERENCES "public"."mcp_execution"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "mcp_admission_deployment_idx" ON "mcp_admission" USING btree ("deployment_id","state","expires_at");--> statement-breakpoint
CREATE INDEX "mcp_admission_connection_idx" ON "mcp_admission" USING btree ("connection_id","state");--> statement-breakpoint
CREATE INDEX "mcp_admission_session_idx" ON "mcp_admission" USING btree ("session_id","state");--> statement-breakpoint
CREATE INDEX "mcp_approval_pending_idx" ON "mcp_approval" USING btree ("deployment_id","decision","expires_at");--> statement-breakpoint
CREATE INDEX "mcp_approval_session_idx" ON "mcp_approval" USING btree ("session_id","decision");--> statement-breakpoint
CREATE INDEX "mcp_catalog_connection_idx" ON "mcp_catalog" USING btree ("connection_id","created_at");--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_catalog_published_revision_idx" ON "mcp_catalog" USING btree ("connection_id","revision") WHERE kind = 'published';--> statement-breakpoint
CREATE INDEX "mcp_execution_task_session_idx" ON "mcp_execution" USING btree ("task_id","session_id");--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_execution_call_idx" ON "mcp_execution" USING btree ("session_id","tool_call_id");--> statement-breakpoint
CREATE INDEX "mcp_execution_connection_state_idx" ON "mcp_execution" USING btree ("connection_id","session_id","state");--> statement-breakpoint
CREATE INDEX "mcp_execution_catalog_idx" ON "mcp_execution" USING btree ("catalog_id");--> statement-breakpoint
CREATE INDEX "mcp_execution_pending_idx" ON "mcp_execution" USING btree ("state","deadline");--> statement-breakpoint
CREATE UNIQUE INDEX "mcp_execution_intent_idx" ON "mcp_execution" USING btree ("intent_id");--> statement-breakpoint
CREATE FUNCTION mcp_catalog_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'MCP catalogs are immutable';
END;
$$;
--> statement-breakpoint
CREATE TRIGGER mcp_catalog_immutable_update BEFORE UPDATE ON mcp_catalog FOR EACH ROW EXECUTE FUNCTION mcp_catalog_immutable();
--> statement-breakpoint
CREATE FUNCTION mcp_execution_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - ARRAY['state','version','boot_id','intent_id','intent_expires_at','result','result_unavailable','updated_at']) IS DISTINCT FROM
     (to_jsonb(OLD) - ARRAY['state','version','boot_id','intent_id','intent_expires_at','result','result_unavailable','updated_at']) THEN
    RAISE EXCEPTION 'MCP request identity is immutable';
  END IF;
  IF NOT ((OLD.state = 'waiting_approval' AND NEW.state IN ('authorized','canceled')) OR
          (OLD.state = 'authorized' AND NEW.state IN ('dispatching','canceled')) OR
          (OLD.state = 'dispatching' AND NEW.state IN ('succeeded','failed','outcome_unknown')) OR
          (OLD.state = 'outcome_unknown' AND NEW.state IN ('succeeded','failed'))) OR NEW.version <> OLD.version + 1 THEN
    RAISE EXCEPTION 'Invalid MCP execution transition';
  END IF;
  IF OLD.intent_id IS NOT NULL AND (NEW.intent_id, NEW.boot_id, NEW.intent_expires_at) IS DISTINCT FROM (OLD.intent_id, OLD.boot_id, OLD.intent_expires_at) THEN
    RAISE EXCEPTION 'MCP dispatch intent is immutable';
  END IF;
  RETURN NEW;
END;
$$;
--> statement-breakpoint
CREATE TRIGGER mcp_execution_transition_update BEFORE UPDATE ON mcp_execution FOR EACH ROW EXECUTE FUNCTION mcp_execution_transition();
