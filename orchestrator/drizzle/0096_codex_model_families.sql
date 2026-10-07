-- The built-in Codex harness model options are model families now (`sol`,
-- `astra`, `luna`). The version-pinned ids are gone, so move every stored
-- native selection to the family of the same tier: the small, fast models
-- (Luna, Mini, Spark) go to `luna`, and all others go to the default `sol`.
-- Without this, a launch that names an old id fails with "model is not
-- valid for harness". Router model ids belong to the router catalog and do
-- not change.
UPDATE "profile"
SET "model" = CASE WHEN "model" IN ('gpt-5.6-luna', 'gpt-5.4-mini', 'gpt-5.3-codex-spark')
                   THEN 'luna' ELSE 'sol' END,
    "updated_at" = CURRENT_TIMESTAMP
WHERE "harness" = 'codex' AND "model_router" IS NULL
  AND "model" IN ('gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna', 'gpt-5.5',
                  'gpt-5.4', 'gpt-5.4-mini', 'gpt-5.3-codex-spark');--> statement-breakpoint
-- A task row records the selection it ran with; keep its updated_at so the
-- task lists do not reorder.
UPDATE "task"
SET "model" = CASE WHEN "model" IN ('gpt-5.6-luna', 'gpt-5.4-mini', 'gpt-5.3-codex-spark')
                   THEN 'luna' ELSE 'sol' END
WHERE "harness" = 'codex' AND "model_router" IS NULL
  AND "model" IN ('gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna', 'gpt-5.5',
                  'gpt-5.4', 'gpt-5.4-mini', 'gpt-5.3-codex-spark');--> statement-breakpoint
-- Automation session blocks keep the selection in nested JSON (blocks nest
-- under then/else/body and under entrypoints). The jsonb text form is
-- canonical (`"key": "value"`), and a quote inside a JSON string value is
-- escaped, so these matches find only a real `model` member. Router catalog
-- ids carry a vendor prefix (`openai/...`), so they never match exactly.
UPDATE "automation_version"
SET "blocks" = regexp_replace(regexp_replace("blocks"::text,
      '"model": "(gpt-5\.6-luna|gpt-5\.4-mini|gpt-5\.3-codex-spark)"', '"model": "luna"', 'g'),
      '"model": "(gpt-5\.6-sol|gpt-5\.6-terra|gpt-5\.5|gpt-5\.4)"', '"model": "sol"', 'g')::jsonb,
    "entrypoints" = regexp_replace(regexp_replace("entrypoints"::text,
      '"model": "(gpt-5\.6-luna|gpt-5\.4-mini|gpt-5\.3-codex-spark)"', '"model": "luna"', 'g'),
      '"model": "(gpt-5\.6-sol|gpt-5\.6-terra|gpt-5\.5|gpt-5\.4)"', '"model": "sol"', 'g')::jsonb
WHERE "blocks"::text ~ '"model": "gpt-5\.(6-sol|6-terra|6-luna|5|4|4-mini|3-codex-spark)"'
   OR "entrypoints"::text ~ '"model": "gpt-5\.(6-sol|6-terra|6-luna|5|4|4-mini|3-codex-spark)"';--> statement-breakpoint
UPDATE "automation"
SET "block_overrides" = regexp_replace(regexp_replace("block_overrides"::text,
      '"model": "(gpt-5\.6-luna|gpt-5\.4-mini|gpt-5\.3-codex-spark)"', '"model": "luna"', 'g'),
      '"model": "(gpt-5\.6-sol|gpt-5\.6-terra|gpt-5\.5|gpt-5\.4)"', '"model": "sol"', 'g')::jsonb,
    "updated_at" = CURRENT_TIMESTAMP
WHERE "block_overrides"::text ~ '"model": "gpt-5\.(6-sol|6-terra|6-luna|5|4|4-mini|3-codex-spark)"';
