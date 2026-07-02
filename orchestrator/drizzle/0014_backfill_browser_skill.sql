-- ADR 0065: the `playwright` and `playwright-cli` skill bundles were merged into
-- the single `browser` bundle (commits c38e208c / b7a7c13c). New profiles can no
-- longer select the retired names (assertSkillsValid), and the web edit-form
-- prunes unknown skills on save — but profile rows never re-saved still carry the
-- stale names, so sessions launched from them request a skill the coordinator can
-- no longer resolve.
--
-- Rewrite those rows: map each retired name to `browser`, then normalize the list
-- to a set — `skills` names are resolved 1:1 to mount slots, so a duplicate name
-- (including the `browser` a profile may already carry) is meaningless; collapse
-- them, keeping first-occurrence order. Only rows containing a retired name are
-- written (outer guard).
--
-- The SRF argument is wrapped so `jsonb_array_elements_text` only ever sees an
-- array: nothing enforces the `string[]` shape at the DB level (no CHECK; Drizzle
-- types it at compile time only), and feeding the SRF a non-array jsonb raises
-- "cannot extract elements from a scalar", which — since the inner scan is not
-- guaranteed to be filtered before the SRF runs — could abort the whole migration
-- depending on the query plan. The CASE makes that impossible regardless of plan;
-- a non-array row yields zero elements and is left untouched.
UPDATE "profile" AS p
SET "skills" = m.skills
FROM (
  SELECT id, jsonb_agg(name ORDER BY ord) AS skills
  FROM (
    SELECT
      pr.id,
      CASE WHEN e.value IN ('playwright', 'playwright-cli') THEN 'browser' ELSE e.value END AS name,
      MIN(e.ord) AS ord
    FROM "profile" AS pr,
         LATERAL jsonb_array_elements_text(
           CASE WHEN jsonb_typeof(pr."skills") = 'array' THEN pr."skills" ELSE '[]'::jsonb END
         ) WITH ORDINALITY AS e(value, ord)
    GROUP BY pr.id, CASE WHEN e.value IN ('playwright', 'playwright-cli') THEN 'browser' ELSE e.value END
  ) mapped
  GROUP BY id
) m
WHERE p.id = m.id
  AND (p."skills" @> '["playwright"]'::jsonb OR p."skills" @> '["playwright-cli"]'::jsonb);
