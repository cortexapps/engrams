ALTER TABLE "spec_participant" ADD COLUMN "connection_epoch" bigint DEFAULT 0 NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_participant" ADD COLUMN "lease_expires_at" timestamp with time zone;--> statement-breakpoint
UPDATE "spec_participant" SET "lease_expires_at" = "connected_at";--> statement-breakpoint
ALTER TABLE "spec_participant" ALTER COLUMN "lease_expires_at" SET NOT NULL;--> statement-breakpoint
CREATE INDEX "spec_participant_live_idx" ON "spec_participant" USING btree ("spec_id","lease_expires_at");
