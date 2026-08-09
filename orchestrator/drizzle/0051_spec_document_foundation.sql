CREATE TABLE "spec" (
	"id" uuid PRIMARY KEY NOT NULL,
	"org_id" text NOT NULL,
	"owner_user_id" text,
	"session_id" uuid,
	"template_id" uuid NOT NULL,
	"title" text NOT NULL,
	"lifecycle" text NOT NULL,
	"current_doc_seq" bigint DEFAULT 0 NOT NULL,
	"published_checkpoint_id" uuid,
	"published_by" text,
	"published_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_checkpoint" (
	"id" uuid PRIMARY KEY NOT NULL,
	"spec_id" uuid NOT NULL,
	"state" "bytea" NOT NULL,
	"state_vector" "bytea" NOT NULL,
	"rendered_markdown" text NOT NULL,
	"doc_seq" bigint NOT NULL,
	"label" text NOT NULL,
	"author_user_id" text,
	"reason" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_open_question" (
	"id" uuid PRIMARY KEY NOT NULL,
	"spec_id" uuid NOT NULL,
	"section_id" text NOT NULL,
	"text" text NOT NULL,
	"opened_by" text,
	"state" text NOT NULL,
	"resolution_note" text,
	"resolved_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_participant" (
	"spec_id" uuid NOT NULL,
	"client_id" text NOT NULL,
	"user_id" text,
	"connected_at" timestamp with time zone DEFAULT now() NOT NULL,
	"disconnected_at" timestamp with time zone,
	CONSTRAINT "spec_participant_spec_id_client_id_pk" PRIMARY KEY("spec_id","client_id")
);
--> statement-breakpoint
CREATE TABLE "spec_section_state" (
	"spec_id" uuid NOT NULL,
	"section_id" text NOT NULL,
	"state" text NOT NULL,
	"na_reason" text,
	"confirmed_by" text,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "spec_section_state_spec_id_section_id_pk" PRIMARY KEY("spec_id","section_id")
);
--> statement-breakpoint
CREATE TABLE "spec_snapshot" (
	"spec_id" uuid PRIMARY KEY NOT NULL,
	"state" "bytea" NOT NULL,
	"state_vector" "bytea" NOT NULL,
	"covered_seq" bigint NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_template" (
	"id" uuid PRIMARY KEY NOT NULL,
	"org_id" text,
	"name" text NOT NULL,
	"description" text,
	"layers" jsonb NOT NULL,
	"sections" jsonb NOT NULL,
	"stage_flags" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "spec_update_log" (
	"seq" bigint NOT NULL,
	"spec_id" uuid NOT NULL,
	"update" "bytea" NOT NULL,
	"client_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "spec_update_log_spec_id_seq_pk" PRIMARY KEY("spec_id","seq")
);
--> statement-breakpoint
ALTER TABLE "spec" ADD CONSTRAINT "spec_owner_user_id_user_id_fk" FOREIGN KEY ("owner_user_id") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec" ADD CONSTRAINT "spec_template_id_spec_template_id_fk" FOREIGN KEY ("template_id") REFERENCES "public"."spec_template"("id") ON DELETE no action ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec" ADD CONSTRAINT "spec_published_by_user_id_fk" FOREIGN KEY ("published_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_checkpoint" ADD CONSTRAINT "spec_checkpoint_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_checkpoint" ADD CONSTRAINT "spec_checkpoint_author_user_id_user_id_fk" FOREIGN KEY ("author_user_id") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_open_question" ADD CONSTRAINT "spec_open_question_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_open_question" ADD CONSTRAINT "spec_open_question_opened_by_user_id_fk" FOREIGN KEY ("opened_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_participant" ADD CONSTRAINT "spec_participant_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_participant" ADD CONSTRAINT "spec_participant_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_section_state" ADD CONSTRAINT "spec_section_state_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_section_state" ADD CONSTRAINT "spec_section_state_confirmed_by_user_id_fk" FOREIGN KEY ("confirmed_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_snapshot" ADD CONSTRAINT "spec_snapshot_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_update_log" ADD CONSTRAINT "spec_update_log_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_org_updated_idx" ON "spec" USING btree ("org_id","updated_at");--> statement-breakpoint
CREATE INDEX "spec_owner_idx" ON "spec" USING btree ("owner_user_id");--> statement-breakpoint
CREATE INDEX "spec_session_idx" ON "spec" USING btree ("session_id");--> statement-breakpoint
CREATE INDEX "spec_checkpoint_spec_created_idx" ON "spec_checkpoint" USING btree ("spec_id","created_at");--> statement-breakpoint
CREATE INDEX "spec_open_question_spec_section_idx" ON "spec_open_question" USING btree ("spec_id","section_id");--> statement-breakpoint
CREATE INDEX "spec_template_org_idx" ON "spec_template" USING btree ("org_id");