CREATE TABLE "automation_tag" (
	"automation_id" text NOT NULL,
	"tag_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "automation_tag_automation_id_tag_id_pk" PRIMARY KEY("automation_id","tag_id")
);
--> statement-breakpoint
CREATE TABLE "tag" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"color" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "automation_tag" ADD CONSTRAINT "automation_tag_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_tag" ADD CONSTRAINT "automation_tag_tag_id_tag_id_fk" FOREIGN KEY ("tag_id") REFERENCES "public"."tag"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "automation_tag_tag_idx" ON "automation_tag" USING btree ("tag_id");--> statement-breakpoint
CREATE UNIQUE INDEX "tag_name_lower_unique" ON "tag" USING btree (lower("name"));