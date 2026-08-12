CREATE TABLE "spec_ticket_draft" (
	"id" uuid PRIMARY KEY NOT NULL,
	"spec_id" uuid NOT NULL,
	"parent_id" uuid,
	"ordinal" integer NOT NULL,
	"title" text NOT NULL,
	"description" text NOT NULL,
	"section_id" text NOT NULL,
	"depends_on" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"sync_state" text DEFAULT 'draft' NOT NULL,
	"linear_id" text,
	"sync_error" text
);
--> statement-breakpoint
ALTER TABLE "spec_ticket_draft" ADD CONSTRAINT "spec_ticket_draft_spec_id_spec_id_fk" FOREIGN KEY ("spec_id") REFERENCES "public"."spec"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_ticket_draft" ADD CONSTRAINT "spec_ticket_draft_parent_id_fk" FOREIGN KEY ("parent_id") REFERENCES "public"."spec_ticket_draft"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "spec_ticket_draft_tree_idx" ON "spec_ticket_draft" USING btree ("spec_id","parent_id","ordinal");
