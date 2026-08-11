ALTER TABLE "spec" ADD COLUMN "current_semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec" SET "current_semantic_doc_seq" = "current_doc_seq";--> statement-breakpoint
ALTER TABLE "spec" ALTER COLUMN "current_semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec" ALTER COLUMN "current_semantic_doc_seq" SET DEFAULT 0;--> statement-breakpoint
ALTER TABLE "spec_projection" ADD COLUMN "semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_projection" SET "semantic_doc_seq" = "doc_seq";--> statement-breakpoint
ALTER TABLE "spec_projection" ALTER COLUMN "semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_snapshot" ADD COLUMN "covered_semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_snapshot" SET "covered_semantic_doc_seq" = "covered_seq";--> statement-breakpoint
ALTER TABLE "spec_snapshot" ALTER COLUMN "covered_semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_update_log" ADD COLUMN "semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_update_log" SET "semantic_doc_seq" = "seq";--> statement-breakpoint
ALTER TABLE "spec_update_log" ALTER COLUMN "semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
CREATE FUNCTION "spec_legacy_semantic_revision_compat"()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF NEW.current_doc_seq IS DISTINCT FROM OLD.current_doc_seq
    AND NEW.current_semantic_doc_seq IS NOT DISTINCT FROM OLD.current_semantic_doc_seq
    AND current_setting('engrams.semantic_revision_writer', true) IS DISTINCT FROM '1'
  THEN
    NEW.current_semantic_doc_seq := NEW.current_doc_seq;
  END IF;
  RETURN NEW;
END;
$$;--> statement-breakpoint
CREATE TRIGGER "spec_legacy_semantic_revision_compat"
BEFORE UPDATE OF "current_doc_seq" ON "spec"
FOR EACH ROW
EXECUTE FUNCTION "spec_legacy_semantic_revision_compat"();--> statement-breakpoint
CREATE FUNCTION "spec_update_log_legacy_semantic_revision_compat"()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF NEW.semantic_doc_seq IS NULL THEN
    NEW.semantic_doc_seq := NEW.seq;
  END IF;
  RETURN NEW;
END;
$$;--> statement-breakpoint
CREATE TRIGGER "spec_update_log_legacy_semantic_revision_compat"
BEFORE INSERT ON "spec_update_log"
FOR EACH ROW
EXECUTE FUNCTION "spec_update_log_legacy_semantic_revision_compat"();--> statement-breakpoint
CREATE FUNCTION "spec_snapshot_legacy_semantic_revision_compat"()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF TG_OP = 'INSERT' AND NEW.covered_semantic_doc_seq IS NULL THEN
    NEW.covered_semantic_doc_seq := NEW.covered_seq;
  ELSIF TG_OP = 'UPDATE'
    AND NEW.covered_seq IS DISTINCT FROM OLD.covered_seq
    AND NEW.covered_semantic_doc_seq IS NOT DISTINCT FROM OLD.covered_semantic_doc_seq
    AND current_setting('engrams.semantic_revision_writer', true) IS DISTINCT FROM '1'
  THEN
    NEW.covered_semantic_doc_seq := NEW.covered_seq;
  END IF;
  RETURN NEW;
END;
$$;--> statement-breakpoint
CREATE TRIGGER "spec_snapshot_legacy_semantic_revision_compat"
BEFORE INSERT ON "spec_snapshot"
FOR EACH ROW
EXECUTE FUNCTION "spec_snapshot_legacy_semantic_revision_compat"();--> statement-breakpoint
CREATE TRIGGER "spec_snapshot_legacy_semantic_revision_update_compat"
BEFORE UPDATE OF "covered_seq" ON "spec_snapshot"
FOR EACH ROW
EXECUTE FUNCTION "spec_snapshot_legacy_semantic_revision_compat"();--> statement-breakpoint
CREATE FUNCTION "spec_projection_legacy_semantic_revision_compat"()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF TG_OP = 'INSERT' AND NEW.semantic_doc_seq IS NULL THEN
    NEW.semantic_doc_seq := NEW.doc_seq;
  ELSIF TG_OP = 'UPDATE'
    AND NEW.doc_seq IS DISTINCT FROM OLD.doc_seq
    AND NEW.semantic_doc_seq IS NOT DISTINCT FROM OLD.semantic_doc_seq
    AND current_setting('engrams.semantic_revision_writer', true) IS DISTINCT FROM '1'
  THEN
    NEW.semantic_doc_seq := NEW.doc_seq;
  END IF;
  RETURN NEW;
END;
$$;--> statement-breakpoint
CREATE TRIGGER "spec_projection_legacy_semantic_revision_compat"
BEFORE INSERT ON "spec_projection"
FOR EACH ROW
EXECUTE FUNCTION "spec_projection_legacy_semantic_revision_compat"();--> statement-breakpoint
CREATE TRIGGER "spec_projection_legacy_semantic_revision_update_compat"
BEFORE UPDATE OF "doc_seq" ON "spec_projection"
FOR EACH ROW
EXECUTE FUNCTION "spec_projection_legacy_semantic_revision_compat"();
