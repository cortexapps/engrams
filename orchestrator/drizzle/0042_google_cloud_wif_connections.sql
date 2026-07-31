CREATE TABLE "integration_connection" (
	"id" text PRIMARY KEY NOT NULL,
	"alias" text NOT NULL,
	"provider" text NOT NULL,
	"display_name" text NOT NULL,
	"config" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"enabled" boolean DEFAULT false NOT NULL,
	"tested_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "integration_connection_alias_unique" UNIQUE("alias")
);
--> statement-breakpoint
CREATE TABLE "profile_launch_grant" (
	"profile_id" text NOT NULL,
	"principal_id" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "profile_launch_grant_profile_id_principal_id_pk" PRIMARY KEY("profile_id","principal_id")
);
--> statement-breakpoint
ALTER TABLE "profile" ADD COLUMN "integration_grants" jsonb DEFAULT '[]'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "profile" ADD COLUMN "launch_access" text DEFAULT 'organization' NOT NULL;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "integration_grants" jsonb;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "integration_principal_id" text;--> statement-breakpoint
UPDATE "task_session" AS target
SET "integration_principal_id" = source."created_by_user_id"
FROM "task" AS source
WHERE target."task_id" = source."id";--> statement-breakpoint
-- Preserve every existing connector grant as a deterministic named connection.
-- These rows keep the current connector-backed behavior. Google Cloud uses the
-- new WIF-only configuration and does not use this legacy backfill path.
INSERT INTO "integration_connection" (
	"id",
	"alias",
	"provider",
	"display_name",
	"config",
	"enabled"
)
SELECT DISTINCT
	'legacy:' || split_part(capability, ':', 1),
	'legacy-' || split_part(capability, ':', 1),
	split_part(capability, ':', 1),
	initcap(replace(split_part(capability, ':', 1), '_', ' ')) || ' (migrated)',
	'{}'::jsonb,
	true
FROM (
	SELECT jsonb_array_elements_text("profile"."capabilities") AS capability
	FROM "profile"
	UNION
	SELECT jsonb_array_elements_text("task_session"."capabilities") AS capability
	FROM "task_session"
) AS existing_capability
ON CONFLICT ("id") DO NOTHING;--> statement-breakpoint
UPDATE "profile" AS target
SET "integration_grants" = source."grants"
FROM (
	SELECT
		"profile"."id",
		jsonb_agg(
			jsonb_build_object(
				'connectionId', 'legacy:' || split_part(capability, ':', 1),
				'operation', substring(
					split_part(capability, '@', 1)
					from position(':' IN split_part(capability, '@', 1)) + 1
				),
				'resourceConstraints', CASE
					WHEN position('@' IN capability) > 0
					THEN jsonb_build_array(substring(capability from position('@' IN capability) + 1))
					ELSE '[]'::jsonb
				END
			)
			ORDER BY capability
		) AS "grants"
	FROM "profile"
	CROSS JOIN LATERAL jsonb_array_elements_text("profile"."capabilities") AS capability
	GROUP BY "profile"."id"
) AS source
WHERE target."id" = source."id";--> statement-breakpoint
UPDATE "task_session" AS target
SET "integration_grants" = source."grants"
FROM (
	SELECT
		"task_session"."task_id",
		"task_session"."session_id",
		jsonb_agg(
			jsonb_build_object(
				'connectionId', 'legacy:' || split_part(capability, ':', 1),
				'operation', substring(
					split_part(capability, '@', 1)
					from position(':' IN split_part(capability, '@', 1)) + 1
				),
				'resourceConstraints', CASE
					WHEN position('@' IN capability) > 0
					THEN jsonb_build_array(substring(capability from position('@' IN capability) + 1))
					ELSE '[]'::jsonb
				END
			)
			ORDER BY capability
		) AS "grants"
	FROM "task_session"
	CROSS JOIN LATERAL jsonb_array_elements_text("task_session"."capabilities") AS capability
	GROUP BY "task_session"."task_id", "task_session"."session_id"
) AS source
WHERE target."task_id" = source."task_id"
	AND target."session_id" = source."session_id";--> statement-breakpoint
ALTER TABLE "profile_launch_grant" ADD CONSTRAINT "profile_launch_grant_profile_id_profile_id_fk" FOREIGN KEY ("profile_id") REFERENCES "public"."profile"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "integration_connection_provider_idx" ON "integration_connection" USING btree ("provider");--> statement-breakpoint
CREATE INDEX "profile_launch_grant_principal_idx" ON "profile_launch_grant" USING btree ("principal_id");--> statement-breakpoint
ALTER TABLE "profile" DROP COLUMN "capabilities";
