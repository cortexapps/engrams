CREATE TABLE "port_exposure" (
	"slug" text PRIMARY KEY NOT NULL,
	"session_id" text NOT NULL,
	"port" integer NOT NULL,
	"label" text DEFAULT '' NOT NULL,
	"owner_user_id" text NOT NULL,
	"visibility" text DEFAULT 'private' NOT NULL,
	"share_token" text,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"expires_at" timestamp
);
--> statement-breakpoint
ALTER TABLE "port_exposure" ADD CONSTRAINT "port_exposure_owner_user_id_user_id_fk" FOREIGN KEY ("owner_user_id") REFERENCES "public"."user"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "port_exposure_session_port_idx" ON "port_exposure" USING btree ("session_id","port");--> statement-breakpoint
CREATE INDEX "port_exposure_session_idx" ON "port_exposure" USING btree ("session_id");--> statement-breakpoint
CREATE INDEX "port_exposure_owner_idx" ON "port_exposure" USING btree ("owner_user_id");