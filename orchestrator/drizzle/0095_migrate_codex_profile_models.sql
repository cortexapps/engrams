-- Move removed native Codex model selections to the current default.
-- Router model IDs belong to the router catalog and must not change here.
UPDATE "profile"
SET "model" = 'gpt-6.1-sol', "updated_at" = CURRENT_TIMESTAMP
WHERE "harness" = 'codex'
  AND "model_router" IS NULL
  AND "model" IN (
    'gpt-5.6-sol',
    'gpt-5.6-terra',
    'gpt-5.6-luna',
    'gpt-5.5',
    'gpt-5.4',
    'gpt-5.4-mini',
    'gpt-5.3-codex-spark'
  );
