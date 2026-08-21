CREATE TABLE "automation_version" (
	"automation_id" text NOT NULL,
	"version" integer NOT NULL,
	"trigger" jsonb NOT NULL,
	"blocks" jsonb NOT NULL,
	"inputs_schema" jsonb DEFAULT '[]'::jsonb NOT NULL,
	"settings" jsonb NOT NULL,
	"created_by_user_id" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "automation_version_automation_id_version_pk" PRIMARY KEY("automation_id","version")
);
--> statement-breakpoint
CREATE TABLE "automation_step_run" (
	"run_id" text NOT NULL,
	"block_id" text NOT NULL,
	"attempt" integer NOT NULL,
	"status" text DEFAULT 'pending' NOT NULL,
	"inputs" jsonb,
	"outputs" jsonb,
	"error" text,
	"started_at" timestamp with time zone DEFAULT now() NOT NULL,
	"ended_at" timestamp with time zone,
	CONSTRAINT "automation_step_run_run_id_block_id_attempt_pk" PRIMARY KEY("run_id","block_id","attempt")
);
--> statement-breakpoint
CREATE TABLE "automation_session" (
	"session_id" text PRIMARY KEY NOT NULL,
	"run_id" text NOT NULL,
	"block_id" text NOT NULL,
	"role" text DEFAULT 'primary' NOT NULL,
	"keep" boolean DEFAULT true NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "automation_concurrency_claim" (
	"automation_id" text NOT NULL,
	"concurrency_key" text NOT NULL,
	"run_id" text NOT NULL,
	"claimed_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "automation_concurrency_claim_automation_id_concurrency_key_pk" PRIMARY KEY("automation_id","concurrency_key")
);
--> statement-breakpoint
ALTER TABLE "automation" ADD COLUMN "kind" text DEFAULT 'user' NOT NULL;--> statement-breakpoint
ALTER TABLE "automation" ADD COLUMN "builtin_key" text;--> statement-breakpoint
ALTER TABLE "automation" ADD COLUMN "current_version" integer DEFAULT 1 NOT NULL;--> statement-breakpoint
ALTER TABLE "automation" ADD COLUMN "inputs" jsonb DEFAULT '{}'::jsonb NOT NULL;--> statement-breakpoint
ALTER TABLE "automation" ADD COLUMN "end_sessions_on_finish" boolean DEFAULT false NOT NULL;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "version" integer DEFAULT 1 NOT NULL;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "delivery_key" text;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "concurrency_key" text;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "context" jsonb;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "started_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "automation_run" ADD COLUMN "ended_at" timestamp with time zone;--> statement-breakpoint
ALTER TABLE "automation_concurrency_claim" ADD CONSTRAINT "automation_concurrency_claim_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_session" ADD CONSTRAINT "automation_session_run_id_automation_run_id_fk" FOREIGN KEY ("run_id") REFERENCES "public"."automation_run"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_step_run" ADD CONSTRAINT "automation_step_run_run_id_automation_run_id_fk" FOREIGN KEY ("run_id") REFERENCES "public"."automation_run"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "automation_version" ADD CONSTRAINT "automation_version_automation_id_automation_id_fk" FOREIGN KEY ("automation_id") REFERENCES "public"."automation"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "automation_session_run_idx" ON "automation_session" USING btree ("run_id");--> statement-breakpoint
CREATE UNIQUE INDEX "automation_builtin_key_unique" ON "automation" USING btree ("builtin_key") WHERE builtin_key is not null;--> statement-breakpoint
CREATE INDEX "automation_run_concurrency_idx" ON "automation_run" USING btree ("automation_id","concurrency_key","status");--> statement-breakpoint
-- ADR 0119 data rewrite: every stored automation becomes version 1 with one
-- create_session block (the legacy action minus its "kind" discriminator).
INSERT INTO "automation_version" ("automation_id", "version", "trigger", "blocks", "inputs_schema", "settings", "created_by_user_id", "created_at")
SELECT "id", 1, "trigger",
  jsonb_build_array(jsonb_build_object('id', 'create_session', 'type', 'create_session', 'config', ("action" - 'kind'))),
  '[]'::jsonb,
  jsonb_build_object('endSessionsOnFinish', false),
  "created_by_user_id", "created_at"
FROM "automation";--> statement-breakpoint
-- Engine run identities: cron runs derive their delivery key from the
-- occurrence; webhook runs from the provider delivery id. Old runs whose
-- delivery id collides across automations stay unique via the automation_id
-- prefix in the index.
UPDATE "automation_run" SET
  "delivery_key" = CASE
    WHEN "trigger"->>'deliveryId' IS NOT NULL THEN 'webhook:' || ("trigger"->>'deliveryId')
    WHEN "scheduled_for" IS NOT NULL THEN 'cron:' || floor(extract(epoch FROM "scheduled_for"))::text
    ELSE NULL
  END,
  "started_at" = "created_at",
  "ended_at" = CASE WHEN "status" IN ('launched', 'skipped', 'render_failed', 'launch_failed') THEN "created_at" ELSE NULL END;--> statement-breakpoint
-- Status map: launched→completed, skipped→filtered, *_failed→failed. A
-- non-terminal legacy run is force-failed so the rebuilt workflow body never
-- adopts an old-shape DBOS execution.
UPDATE "automation_run" SET
  "error" = CASE WHEN "status" = 'pending' THEN 'migrated: engine rebuild' ELSE "error" END,
  "ended_at" = COALESCE("ended_at", now()),
  "status" = CASE "status"
    WHEN 'launched' THEN 'completed'
    WHEN 'skipped' THEN 'filtered'
    WHEN 'render_failed' THEN 'failed'
    WHEN 'launch_failed' THEN 'failed'
    WHEN 'pending' THEN 'failed'
    ELSE 'failed'
  END;--> statement-breakpoint
CREATE UNIQUE INDEX "automation_run_delivery_unique" ON "automation_run" USING btree ("automation_id","delivery_key") WHERE delivery_key is not null;--> statement-breakpoint
-- One step-run row per legacy run: the single create_session block.
INSERT INTO "automation_step_run" ("run_id", "block_id", "attempt", "status", "outputs", "error", "started_at", "ended_at")
SELECT "id", 'create_session', 0,
  CASE "status" WHEN 'completed' THEN 'succeeded' WHEN 'filtered' THEN 'skipped' ELSE 'failed' END,
  jsonb_strip_nulls(jsonb_build_object('session_id', "session_id", 'task_id', "task_id", 'prompt', "rendered_prompt", 'title', "rendered_title")),
  "error", "created_at", COALESCE("ended_at", "created_at")
FROM "automation_run";--> statement-breakpoint
-- Session bindings for legacy runs: kept alive, matching the old behavior.
INSERT INTO "automation_session" ("session_id", "run_id", "block_id", "role", "keep", "created_at")
SELECT DISTINCT ON ("session_id") "session_id", "id", 'create_session', 'primary', true, "created_at"
FROM "automation_run"
WHERE "session_id" IS NOT NULL
ORDER BY "session_id", "created_at" DESC
ON CONFLICT ("session_id") DO NOTHING;--> statement-breakpoint
ALTER TABLE "automation" DROP COLUMN "trigger";--> statement-breakpoint
ALTER TABLE "automation" DROP COLUMN "action";
