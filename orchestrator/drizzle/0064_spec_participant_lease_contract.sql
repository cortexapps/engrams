-- Contract phase for migration 0055. Every orchestrator pod now writes an
-- explicit finite lease and increments the connection epoch, so the
-- compatibility default, trigger, and function are no longer necessary.
DROP TRIGGER "spec_participant_legacy_lease_compat" ON "spec_participant";
--> statement-breakpoint
DROP FUNCTION "spec_participant_legacy_lease_compat"();
--> statement-breakpoint
ALTER TABLE "spec_participant"
  ALTER COLUMN "lease_expires_at" DROP DEFAULT;
--> statement-breakpoint
-- Expire the infinite leases that the contract gave to old-pod rows. Their
-- stale presence disappears, and no current writer holds an epoch-zero row.
UPDATE "spec_participant"
SET "lease_expires_at" = '-infinity'::timestamptz
WHERE "connection_epoch" = 0
  AND "lease_expires_at" = 'infinity'::timestamptz;
