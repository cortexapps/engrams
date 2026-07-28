CREATE TABLE "review_target" (
	"id" uuid PRIMARY KEY DEFAULT gen_random_uuid() NOT NULL,
	"provider" text DEFAULT 'github' NOT NULL,
	"provider_id" text,
	"repo" text NOT NULL,
	"number" integer NOT NULL,
	"title" text,
	"author" text,
	"state" text,
	"url" text,
	"provider_updated_at" timestamp with time zone,
	"hydration_failed_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
ALTER TABLE "review" ADD COLUMN "target_id" uuid;--> statement-breakpoint
CREATE UNIQUE INDEX "review_target_provider_id_unique" ON "review_target" USING btree ("provider","provider_id");--> statement-breakpoint
CREATE INDEX "review_target_coordinate_idx" ON "review_target" USING btree ("provider","repo","number");--> statement-breakpoint
ALTER TABLE "review" ADD CONSTRAINT "review_target_id_review_target_id_fk" FOREIGN KEY ("target_id") REFERENCES "public"."review_target"("id") ON DELETE cascade ON UPDATE no action;--> statement-breakpoint
CREATE INDEX "review_target_idx" ON "review" USING btree ("target_id");--> statement-breakpoint
-- Backfill: one target per distinct (repo, pr_number). Each descriptive field
-- takes the newest NON-NULL value across that PR's passes, PER FIELD rather than
-- per row — a pass that failed before capture stored nulls, and letting it win
-- just because it is newest would erase a name we already have. That is the bug
-- this table exists to fix, so the backfill must not reintroduce it.
--
-- `id DESC` breaks a timestamp tie so the choice is deterministic; without it two
-- passes written in the same instant have no defined order.
--
-- `provider_id`, `url` and `provider_updated_at` stay NULL: nothing in `review`
-- can produce them, and a synthesized value would be indistinguishable from one
-- the forge actually gave us. The hydrator fills them in from the API.
INSERT INTO "review_target" ("provider", "repo", "number", "title", "author", "state", "created_at", "updated_at")
SELECT
	'github',
	"repo",
	"pr_number",
	(array_remove(array_agg("pr_title" ORDER BY "created_at" DESC, "id" DESC), NULL))[1],
	(array_remove(array_agg("pr_author" ORDER BY "created_at" DESC, "id" DESC), NULL))[1],
	(array_remove(array_agg("pr_state" ORDER BY "created_at" DESC, "id" DESC), NULL))[1],
	min("created_at"),
	max("updated_at")
FROM "review"
GROUP BY "repo", "pr_number";--> statement-breakpoint
UPDATE "review" AS r SET "target_id" = t."id"
FROM "review_target" AS t
WHERE t."provider" = 'github' AND t."repo" = r."repo" AND t."number" = r."pr_number";