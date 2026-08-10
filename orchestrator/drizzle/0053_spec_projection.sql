CREATE TABLE "spec_projection" (
	"spec_id" uuid NOT NULL,
	"rev" bigint NOT NULL,
	"session_id" uuid NOT NULL,
	"doc_seq" bigint NOT NULL,
	"sha256" text NOT NULL,
	"rendered" "bytea" NOT NULL,
	"document_state" "bytea" NOT NULL,
	"digest" "bytea" NOT NULL,
	"digest_sha256" text NOT NULL,
	"staging_path" text NOT NULL,
	"state" text NOT NULL,
	"requested_source" text NOT NULL,
	"discard_notice" boolean DEFAULT false NOT NULL,
	"pushed_at" timestamp with time zone,
	"created_at" timestamp with time zone NOT NULL,
	CONSTRAINT "spec_projection_spec_id_rev_pk" PRIMARY KEY("spec_id","rev")
);
--> statement-breakpoint
ALTER TABLE "spec_projection" ADD CONSTRAINT "spec_projection_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_projection_session_state_idx" ON "spec_projection" USING btree ("session_id","state","rev");