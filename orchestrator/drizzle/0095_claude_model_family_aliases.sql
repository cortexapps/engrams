-- The built-in Claude harness model options are family aliases now (`opus`,
-- `fable`, `sonnet`, `haiku`). The Claude Code CLI resolves each alias to the
-- latest model of that family. The version-pinned ids `opus-5` and `fable-5`
-- are gone, so move every stored native selection to its family id. Without
-- this, a launch that names an old id fails with "model is not valid for
-- harness". Router model ids belong to the router catalog and do not change.
UPDATE "profile"
SET "model" = CASE "model" WHEN 'opus-5' THEN 'opus' ELSE 'fable' END,
    "updated_at" = CURRENT_TIMESTAMP
WHERE "harness" = 'claude' AND "model_router" IS NULL
  AND "model" IN ('opus-5', 'fable-5');--> statement-breakpoint
-- A task row records the selection it ran with; keep its updated_at so the
-- task lists do not reorder.
UPDATE "task"
SET "model" = CASE "model" WHEN 'opus-5' THEN 'opus' ELSE 'fable' END
WHERE "harness" = 'claude' AND "model_router" IS NULL
  AND "model" IN ('opus-5', 'fable-5');--> statement-breakpoint
-- Automation session blocks keep the selection in nested JSON (blocks nest
-- under then/else/body and under entrypoints). The jsonb text form is
-- canonical (`"key": "value"`), and a quote inside a JSON string value is
-- escaped, so this match finds only a real `model` member. No harness other
-- than claude, and no router catalog, uses the ids `opus-5` or `fable-5`.
UPDATE "automation_version"
SET "blocks" = replace(replace("blocks"::text,
      '"model": "opus-5"', '"model": "opus"'),
      '"model": "fable-5"', '"model": "fable"')::jsonb,
    "entrypoints" = replace(replace("entrypoints"::text,
      '"model": "opus-5"', '"model": "opus"'),
      '"model": "fable-5"', '"model": "fable"')::jsonb
WHERE "blocks"::text ~ '"model": "(opus|fable)-5"'
   OR "entrypoints"::text ~ '"model": "(opus|fable)-5"';--> statement-breakpoint
UPDATE "automation"
SET "block_overrides" = replace(replace("block_overrides"::text,
      '"model": "opus-5"', '"model": "opus"'),
      '"model": "fable-5"', '"model": "fable"')::jsonb,
    "updated_at" = CURRENT_TIMESTAMP
WHERE "block_overrides"::text ~ '"model": "(opus|fable)-5"';
