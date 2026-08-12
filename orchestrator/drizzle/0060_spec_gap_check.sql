CREATE TABLE "spec_gap_check_run" (
	"id" uuid PRIMARY KEY NOT NULL,
	"spec_id" uuid NOT NULL,
	"session_id" uuid,
	"request_fingerprint" text NOT NULL,
	"semantic_doc_seq" bigint NOT NULL,
	"stopped_at_layer_key" text,
	"suppressed_count" integer DEFAULT 0 NOT NULL,
	"matrix" jsonb NOT NULL,
	"started_by" text,
	"created_at" timestamp with time zone NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_gap_check_finding" (
	"run_id" uuid NOT NULL,
	"finding_id" text NOT NULL,
	"ordinal" integer NOT NULL,
	"kind" text NOT NULL,
	"severity" text NOT NULL,
	"layer_key" text NOT NULL,
	"section_id" text NOT NULL,
	"section_title" text NOT NULL,
	"requirement_id" text,
	"summary" text NOT NULL,
	"detail" text NOT NULL,
	"proposed_diff" jsonb,
	"disposition" text DEFAULT 'pending' NOT NULL,
	"open_question_id" uuid,
	"disposed_by" text,
	"disposed_at" timestamp with time zone,
	CONSTRAINT "spec_gap_check_finding_run_id_finding_id_pk" PRIMARY KEY("run_id","finding_id")
);
--> statement-breakpoint
ALTER TABLE "spec_gap_check_run" ADD CONSTRAINT "spec_gap_check_run_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_gap_check_run" ADD CONSTRAINT "spec_gap_check_run_started_by_user_id_fk" FOREIGN KEY ("started_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_gap_check_finding" ADD CONSTRAINT "spec_gap_check_finding_run_id_spec_gap_check_run_id_fk" FOREIGN KEY ("run_id") REFERENCES "public"."spec_gap_check_run"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_gap_check_finding" ADD CONSTRAINT "spec_gap_check_finding_disposed_by_user_id_fk" FOREIGN KEY ("disposed_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
CREATE UNIQUE INDEX "spec_gap_check_run_fingerprint_idx" ON "spec_gap_check_run" USING btree ("spec_id","request_fingerprint");--> statement-breakpoint
CREATE INDEX "spec_gap_check_run_spec_created_idx" ON "spec_gap_check_run" USING btree ("spec_id","created_at");
