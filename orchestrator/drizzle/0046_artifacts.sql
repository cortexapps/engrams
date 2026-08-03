CREATE TABLE "artifact" (
	"id" text PRIMARY KEY NOT NULL,
	"owner_user_id" text,
	"title" text NOT NULL,
	"file_name" text NOT NULL,
	"visibility" text DEFAULT 'private' NOT NULL,
	"current_version" integer NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "artifact_version" (
	"artifact_id" text NOT NULL,
	"version" integer NOT NULL,
	"session_id" text NOT NULL,
	"task_id" text,
	"coord_artifact_id" text NOT NULL,
	"media_type" text NOT NULL,
	"size_bytes" bigint NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL,
	CONSTRAINT "artifact_version_artifact_id_version_pk" PRIMARY KEY("artifact_id","version")
);
--> statement-breakpoint
ALTER TABLE "artifact" ADD CONSTRAINT "artifact_owner_user_id_user_id_fk" FOREIGN KEY ("owner_user_id") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "artifact_version" ADD CONSTRAINT "artifact_version_artifact_id_artifact_id_fk" FOREIGN KEY ("artifact_id") REFERENCES "public"."artifact"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "artifact_owner_idx" ON "artifact" USING btree ("owner_user_id");--> statement-breakpoint
CREATE INDEX "artifact_visibility_idx" ON "artifact" USING btree ("visibility");--> statement-breakpoint
CREATE INDEX "artifact_version_session_idx" ON "artifact_version" USING btree ("session_id");