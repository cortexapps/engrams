-- ADR 0119 phase 4.7: the reviewer profile seed and its `pr_reviewer`
-- designation retire. The PR-review built-in's `profile` input names the
-- reviewer profile by id now, so copy the designated profile's id into it
-- (the built-in's input held the literal designation). With no designated
-- profile the input is emptied: reviews stay off until one is picked on
-- the Reviews page.
UPDATE "automation" a
SET "inputs" = jsonb_set(a."inputs", '{profile}', to_jsonb(p."id"), true),
    "updated_at" = now()
FROM "profile" p
WHERE p."designation" = 'pr_reviewer' AND p."deleted_at" IS NULL
  AND a."builtin_key" = 'pr_review' AND a."archived_at" IS NULL;--> statement-breakpoint
UPDATE "automation"
SET "inputs" = jsonb_set("inputs", '{profile}', '""'::jsonb, true),
    "updated_at" = now()
WHERE "builtin_key" = 'pr_review' AND "archived_at" IS NULL
  AND "inputs"->>'profile' = 'pr_reviewer';--> statement-breakpoint
DROP INDEX "profile_designation_unique";--> statement-breakpoint
ALTER TABLE "profile" DROP COLUMN "designation";