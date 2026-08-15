-- ADR 0118: retire the port exposure; add the session app.
--
-- An exposure was an opaque slug over one `(session, port)` pair, minted after
-- the session was already live. A session app is a NAMED service whose hostname
-- is reserved BEFORE the session exists, so the address can be injected into the
-- guest's environment and a sibling app can be configured with an address the
-- user's browser can also reach.
--
-- This is a clean break, not a migration. The two models differ in their primary
-- key (`slug` vs `<name>-<session slug>`), so no existing row has a hostname the
-- new routing key would produce, and any carried-over row would resolve to an
-- address nothing serves. Existing preview links stop working, which is the
-- accepted cost of one naming scheme instead of two.
--
-- Dropped along with the table:
--   * `share_token` — an unauthenticated capability in a URL, which is exactly
--     the way around the login wall that ADR 0118 exists to close.
--   * `expires_at`  — enforced on read but never written by anything.

DROP TABLE IF EXISTS "port_exposure";--> statement-breakpoint

CREATE TABLE "session_app" (
	"host_label" text PRIMARY KEY NOT NULL,
	"session_id" text NOT NULL,
	"name" text NOT NULL,
	"port" integer NOT NULL,
	"owner_user_id" text NOT NULL,
	"visibility" text DEFAULT 'org' NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL
);--> statement-breakpoint

ALTER TABLE "session_app" ADD CONSTRAINT "session_app_owner_user_id_user_id_fk" FOREIGN KEY ("owner_user_id") REFERENCES "public"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint

-- One app per name and one per port. Both are what make the create-time
-- reservation idempotent: a retried create inserts nothing and reads back the
-- hostnames that already exist.
CREATE UNIQUE INDEX "session_app_session_name_idx" ON "session_app" USING btree ("session_id","name");--> statement-breakpoint
CREATE UNIQUE INDEX "session_app_session_port_idx" ON "session_app" USING btree ("session_id","port");--> statement-breakpoint
CREATE INDEX "session_app_session_idx" ON "session_app" USING btree ("session_id");--> statement-breakpoint
CREATE INDEX "session_app_owner_idx" ON "session_app" USING btree ("owner_user_id");--> statement-breakpoint

-- profile.port_exposures (uint32[]) → profile.apps ([{name, port}]).
-- Every declaration is a bare port with no name, so there is nothing to carry
-- over; the column starts empty and profiles are re-declared.
ALTER TABLE "profile" DROP COLUMN IF EXISTS "port_exposures";--> statement-breakpoint
ALTER TABLE "profile" ADD COLUMN "apps" jsonb DEFAULT '[]'::jsonb NOT NULL;
