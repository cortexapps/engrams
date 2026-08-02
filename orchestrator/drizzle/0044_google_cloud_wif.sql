CREATE TABLE "integration_connection" (
	"id" text PRIMARY KEY NOT NULL,
	"alias" text NOT NULL,
	"provider" text NOT NULL,
	"display_name" text NOT NULL,
	"is_default" boolean DEFAULT false NOT NULL,
	"config" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"enabled" boolean DEFAULT false NOT NULL,
	"tested_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "integration_connection_alias_unique" UNIQUE("alias")
);
--> statement-breakpoint
CREATE TABLE "integration_oidc_key" (
	"kid" text PRIMARY KEY NOT NULL,
	"public_jwk" jsonb NOT NULL,
	"wrapped_dek" "bytea" NOT NULL,
	"nonce" "bytea" NOT NULL,
	"ciphertext" "bytea" NOT NULL,
	"key_id" text NOT NULL,
	"state" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"publish_until" timestamp with time zone
);
--> statement-breakpoint
ALTER TABLE "profile" ADD COLUMN "integration_grants" jsonb DEFAULT '[]'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "integration_grants" jsonb;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "integration_connections" jsonb;--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "integration_principal_id" text;--> statement-breakpoint
UPDATE "task_session" AS target
SET "integration_principal_id" = source."created_by_user_id"
FROM "task" AS source
WHERE target."task_id" = source."id";--> statement-breakpoint
-- Give every provider referenced by existing authority one ordinary default
-- connection. The opaque ID carries no provider or migration semantics.
INSERT INTO "integration_connection" (
	"id",
	"alias",
	"provider",
	"display_name",
	"is_default",
	"config",
	"enabled"
)
SELECT
	gen_random_uuid()::text,
	provider || '-default',
	provider,
	initcap(replace(provider, '_', ' ')) || ' (default)',
	true,
	'{}'::jsonb,
	true
FROM (
	SELECT DISTINCT split_part(capability, ':', 1) AS provider
	FROM (
		SELECT jsonb_array_elements_text("profile"."capabilities") AS capability
		FROM "profile"
		UNION
		SELECT jsonb_array_elements_text("task_session"."capabilities") AS capability
		FROM "task_session"
	) AS existing_capability
) AS existing_provider
ON CONFLICT ("alias") DO NOTHING;--> statement-breakpoint
UPDATE "profile" AS target
SET "integration_grants" = source."grants"
FROM (
	SELECT
		"profile"."id",
		jsonb_agg(
			jsonb_build_object(
				'connectionId', (
					SELECT "id"
					FROM "integration_connection"
					WHERE "provider" = split_part(capability, ':', 1)
						AND "is_default"
				),
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
				'connectionId', (
					SELECT "id"
					FROM "integration_connection"
					WHERE "provider" = split_part(capability, ':', 1)
						AND "is_default"
				),
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
CREATE INDEX "integration_connection_provider_idx" ON "integration_connection" USING btree ("provider");--> statement-breakpoint
CREATE UNIQUE INDEX "integration_connection_provider_default_unique" ON "integration_connection" USING btree ("provider") WHERE is_default;--> statement-breakpoint
CREATE UNIQUE INDEX "integration_oidc_key_active_unique" ON "integration_oidc_key" USING btree ("state") WHERE state = 'active';--> statement-breakpoint
ALTER TABLE "profile" DROP COLUMN "capabilities";
