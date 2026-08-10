-- Expand contract for pre-#1146 orchestrator pods. Old inserts omit the lease,
-- and old reconnect upserts leave the finite lease unchanged. Keep this
-- compatibility until #1153 removes it after all old pods are absent.
ALTER TABLE "spec_participant"
  ALTER COLUMN "lease_expires_at" SET DEFAULT 'infinity'::timestamptz;
--> statement-breakpoint
CREATE FUNCTION "spec_participant_legacy_lease_compat"()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF NEW.disconnected_at IS NULL THEN
    -- A legacy reconnect can claim only an inactive row. A reconnect against
    -- any active owner is a no-op, including a current owner with an epoch.
    IF OLD.disconnected_at IS NULL THEN
      RETURN OLD;
    END IF;

    NEW.connection_epoch := 0;
    NEW.lease_expires_at := 'infinity'::timestamptz;
    RETURN NEW;
  END IF;

  -- A legacy disconnect has no epoch predicate. Do not let it disconnect a
  -- row after a current writer has minted an epoch for the replacement socket.
  IF OLD.connection_epoch > 0
    AND OLD.disconnected_at IS NULL
    AND NEW.disconnected_at IS NOT NULL
  THEN
    RETURN OLD;
  END IF;

  RETURN NEW;
END;
$$;
--> statement-breakpoint
CREATE TRIGGER "spec_participant_legacy_lease_compat"
BEFORE UPDATE OF "connected_at", "disconnected_at" ON "spec_participant"
FOR EACH ROW
WHEN (
  NEW."connection_epoch" = OLD."connection_epoch"
  AND NEW."lease_expires_at" = OLD."lease_expires_at"
)
EXECUTE FUNCTION "spec_participant_legacy_lease_compat"();
--> statement-breakpoint
UPDATE "spec_participant"
SET "lease_expires_at" = 'infinity'::timestamptz
WHERE "connection_epoch" = 0
  AND "disconnected_at" IS NULL;
