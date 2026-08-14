ALTER TABLE "profile" ADD COLUMN "model_router" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "model_router" text;--> statement-breakpoint

CREATE TABLE "router_model" (
  "router_id" text NOT NULL,
  "model_id" text NOT NULL,
  "canonical_slug" text NOT NULL,
  "name" text NOT NULL,
  "author" text,
  "description" text,
  "context_length" integer DEFAULT 0 NOT NULL,
  "prompt_price" text,
  "completion_price" text,
  "input_modalities" jsonb DEFAULT '[]'::jsonb NOT NULL,
  "output_modalities" jsonb DEFAULT '[]'::jsonb NOT NULL,
  "supported_parameters" jsonb DEFAULT '[]'::jsonb NOT NULL,
  "hugging_face_id" text,
  "upstream" jsonb DEFAULT '{}'::jsonb NOT NULL,
  "available" boolean DEFAULT true NOT NULL,
  "updated_at" timestamp with time zone DEFAULT now() NOT NULL,
  CONSTRAINT "router_model_router_id_model_id_pk" PRIMARY KEY("router_id", "model_id")
);--> statement-breakpoint
CREATE INDEX "router_model_available_idx" ON "router_model" ("router_id", "available");--> statement-breakpoint

CREATE TABLE "router_model_policy" (
  "router_id" text NOT NULL,
  "model_id" text NOT NULL,
  "enabled" boolean DEFAULT false NOT NULL,
  "user_enabled" boolean DEFAULT false NOT NULL,
  "updated_at" timestamp with time zone DEFAULT now() NOT NULL,
  CONSTRAINT "router_model_policy_router_id_model_id_pk" PRIMARY KEY("router_id", "model_id"),
  CONSTRAINT "router_model_policy_model_fk" FOREIGN KEY ("router_id", "model_id") REFERENCES "router_model"("router_id", "model_id") ON DELETE CASCADE,
  CONSTRAINT "router_model_policy_user_implies_enabled" CHECK (NOT "user_enabled" OR "enabled")
);--> statement-breakpoint

CREATE TABLE "router_sync_state" (
  "router_id" text PRIMARY KEY NOT NULL,
  "last_successful_sync_at" timestamp with time zone,
  "last_attempt_at" timestamp with time zone,
  "last_error" text
);--> statement-breakpoint

INSERT INTO "router_model" ("router_id", "model_id", "canonical_slug", "name", "author", "available", "supported_parameters", "output_modalities") VALUES
  ('openrouter', 'z-ai/glm-5.2', 'z-ai/glm-5.2', 'GLM 5.2', 'z-ai', true, '["tools"]', '["text"]'),
  ('openrouter', 'deepseek/deepseek-v4-flash-0731', 'deepseek/deepseek-v4-flash-0731', 'DeepSeek V4 Flash 0731', 'deepseek', true, '["tools"]', '["text"]'),
  ('openrouter', 'deepseek/deepseek-v4-pro-0813', 'deepseek/deepseek-v4-pro-20260813', 'DeepSeek V4 Pro 0813', 'deepseek', true, '["tools", "reasoning", "reasoning_effort"]', '["text"]');--> statement-breakpoint
INSERT INTO "router_model_policy" ("router_id", "model_id", "enabled", "user_enabled")
  SELECT "router_id", "model_id", true, true FROM "router_model";--> statement-breakpoint

UPDATE "profile" SET "model_router" = 'openrouter', "model" = 'z-ai/glm-5.2'
  WHERE "harness" = 'claude' AND "model" = 'glm-5.2';--> statement-breakpoint
UPDATE "profile" SET "model_router" = 'openrouter', "model" = 'deepseek/deepseek-v4-flash-0731'
  WHERE "harness" = 'claude' AND "model" = 'deepseek-v4-flash';--> statement-breakpoint
UPDATE "task" SET "model_router" = 'openrouter', "model" = 'z-ai/glm-5.2'
  WHERE "harness" = 'claude' AND "model" = 'glm-5.2';--> statement-breakpoint
UPDATE "task" SET "model_router" = 'openrouter', "model" = 'deepseek/deepseek-v4-flash-0731'
  WHERE "harness" = 'claude' AND "model" = 'deepseek-v4-flash';--> statement-breakpoint

UPDATE "task" SET "launch_policy" = jsonb_set(jsonb_set("launch_policy", '{modelRouter}', '"openrouter"'), '{model}', '"z-ai/glm-5.2"')
  WHERE ("launch_policy"->>'harness' IS NULL OR "launch_policy"->>'harness' = 'claude') AND "launch_policy"->>'model' = 'glm-5.2';--> statement-breakpoint
UPDATE "task" SET "launch_policy" = jsonb_set(jsonb_set("launch_policy", '{modelRouter}', '"openrouter"'), '{model}', '"deepseek/deepseek-v4-flash-0731"')
  WHERE ("launch_policy"->>'harness' IS NULL OR "launch_policy"->>'harness' = 'claude') AND "launch_policy"->>'model' = 'deepseek-v4-flash';--> statement-breakpoint

UPDATE "automation" SET "action" = jsonb_set(jsonb_set("action", '{modelRouter}', '"openrouter"'), '{model}', '"z-ai/glm-5.2"')
  WHERE ("action"->>'harness' IS NULL OR "action"->>'harness' = 'claude') AND "action"->>'model' = 'glm-5.2';--> statement-breakpoint
UPDATE "automation" SET "action" = jsonb_set(jsonb_set("action", '{modelRouter}', '"openrouter"'), '{model}', '"deepseek/deepseek-v4-flash-0731"')
  WHERE ("action"->>'harness' IS NULL OR "action"->>'harness' = 'claude') AND "action"->>'model' = 'deepseek-v4-flash';
