ALTER TABLE "mcp_discovery_admission" ADD COLUMN "operation" jsonb DEFAULT '{"kind":"discover"}'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "mcp_managed_lifecycle" jsonb;--> statement-breakpoint
ALTER TABLE "mcp_discovery_admission" ADD CONSTRAINT "mcp_administrative_operation_check" CHECK (coalesce(
      "mcp_discovery_admission"."operation" = '{"kind":"discover"}'::jsonb OR (
        "mcp_discovery_admission"."operation"->>'kind' = 'validateSchemas' AND
        "mcp_discovery_admission"."operation"->>'documentsDigest' ~ '^sha256:[0-9a-f]{64}$' AND
        "mcp_discovery_admission"."operation" - 'kind' - 'documentsDigest' = '{}'::jsonb AND "mcp_discovery_admission"."destination" = ''
      ), false));--> statement-breakpoint
CREATE FUNCTION mcp_managed_lifecycle_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.mcp_managed_lifecycle IS NOT NULL AND (
    NEW.mcp_managed_lifecycle IS NULL OR
    NEW.mcp_managed_lifecycle->'identity' IS DISTINCT FROM OLD.mcp_managed_lifecycle->'identity' OR
    NEW.mcp_managed_lifecycle->'nativeVersion' IS DISTINCT FROM OLD.mcp_managed_lifecycle->'nativeVersion' OR
    NEW.mcp_managed_lifecycle->'nativeDigest' IS DISTINCT FROM OLD.mcp_managed_lifecycle->'nativeDigest'
  ) THEN
    RAISE EXCEPTION 'MCP creation identity is immutable';
  END IF;
  RETURN NEW;
END;
$$;
--> statement-breakpoint
CREATE TRIGGER mcp_managed_lifecycle_identity BEFORE UPDATE ON task_session
  FOR EACH ROW EXECUTE FUNCTION mcp_managed_lifecycle_identity();
