-- ADR 0119 phase 4.7: the legacy review graph is gone, so every enrolled
-- repo reviews on the PR-review built-in. Lift each enrollment row into the
-- built-in's `repos` input (an entry the map already carries wins: it was
-- written by the product) and turn the built-in on when anything is
-- enrolled, so a repo that was still on the legacy graph keeps getting
-- reviews after this deploy with nobody saving it again.
UPDATE "automation" a
SET "inputs" = jsonb_set(
      a."inputs" || jsonb_build_object('repos', coalesce(a."inputs"->'repos', '{}'::jsonb)),
      '{repos}',
      lifted.repos || coalesce(a."inputs"->'repos', '{}'::jsonb),
      true
    ),
    "enabled" = true,
    "updated_at" = now()
FROM (
  SELECT jsonb_object_agg(
           "repo",
           jsonb_build_object(
             'mode', CASE WHEN "trigger_mode" = 'auto' THEN 'auto' ELSE 'on_request' END,
             'autofix', "autofix" <> 'off'
           )
         ) AS repos
  FROM "review_enrollment"
) lifted
WHERE a."builtin_key" = 'pr_review' AND a."archived_at" IS NULL AND lifted.repos IS NOT NULL;--> statement-breakpoint
DROP TABLE "review_session" CASCADE;--> statement-breakpoint
ALTER TABLE "review" DROP COLUMN "status_comment_id";--> statement-breakpoint
ALTER TABLE "review_enrollment" DROP COLUMN "engine";