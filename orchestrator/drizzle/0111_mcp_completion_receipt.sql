ALTER TABLE "mcp_execution" ADD COLUMN "completion_enqueued_version" integer DEFAULT 0 NOT NULL;--> statement-breakpoint
CREATE INDEX "mcp_execution_recovery_idx" ON "mcp_execution" USING btree ("id") WHERE "mcp_execution"."completion_enqueued_version" < "mcp_execution"."version";--> statement-breakpoint
ALTER TABLE "mcp_execution" ADD CONSTRAINT "mcp_execution_completion_version_check" CHECK ("mcp_execution"."completion_enqueued_version" >= 0 AND "mcp_execution"."completion_enqueued_version" <= "mcp_execution"."version");
--> statement-breakpoint
CREATE OR REPLACE FUNCTION mcp_execution_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'completion_enqueued_version') = (to_jsonb(OLD) - 'completion_enqueued_version')
     AND OLD.state IN ('succeeded','failed','canceled','outcome_unknown')
     AND NEW.completion_enqueued_version >= OLD.completion_enqueued_version
     AND NEW.completion_enqueued_version <= OLD.version THEN
    RETURN NEW;
  END IF;
  IF (to_jsonb(NEW) - ARRAY['state','version','boot_id','intent_id','intent_expires_at','result','result_unavailable','updated_at']) IS DISTINCT FROM
     (to_jsonb(OLD) - ARRAY['state','version','boot_id','intent_id','intent_expires_at','result','result_unavailable','updated_at']) THEN
    RAISE EXCEPTION 'MCP request identity is immutable';
  END IF;
  IF NOT ((OLD.state = 'waiting_approval' AND NEW.state IN ('authorized','canceled','failed')) OR
          (OLD.state = 'authorized' AND NEW.state IN ('dispatching','canceled','failed')) OR
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
