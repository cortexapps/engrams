ALTER TABLE "spec_template" ALTER COLUMN "stage_flags" SET DEFAULT '{"alternatives":"on","talkItThrough":"suggested","gapCheck":"on"}'::jsonb;--> statement-breakpoint
UPDATE "spec_template"
SET "stage_flags" = jsonb_build_object(
  'alternatives',
  CASE
    WHEN "stage_flags"->>'alternatives' IN ('on', 'suggested', 'off')
      THEN "stage_flags"->>'alternatives'
    WHEN "stage_flags"->>'alternatives' = 'false' THEN 'off'
    ELSE 'on'
  END,
  'talkItThrough',
  CASE
    WHEN "stage_flags"->>'talkItThrough' IN ('on', 'suggested', 'off')
      THEN "stage_flags"->>'talkItThrough'
    WHEN "stage_flags"->>'talkItThrough' = 'true' THEN 'on'
    WHEN "stage_flags"->>'talkItThrough' = 'false' THEN 'off'
    ELSE 'suggested'
  END,
  'gapCheck',
  CASE
    WHEN "stage_flags"->>'gapCheck' IN ('on', 'suggested', 'off')
      THEN "stage_flags"->>'gapCheck'
    WHEN "stage_flags"->>'gapCheck' = 'false' THEN 'off'
    ELSE 'on'
  END
);--> statement-breakpoint
INSERT INTO "spec_template" (
  "id",
  "org_id",
  "name",
  "description",
  "layers",
  "sections",
  "stage_flags"
) VALUES (
  '00000000-0000-4000-8000-000000000115',
  NULL,
  'Engineering design doc',
  'A three-layer design document that moves from intent to system detail.',
  '[{"key":"intent","title":"Intent","description":"Define the problem, the desired outcome, and the constraints."},{"key":"contract","title":"Contract","description":"Define observable behavior and the public contract."},{"key":"system","title":"System","description":"Define the implementation, its boundaries, and its release plan."}]'::jsonb,
  '[{"key":"problem","title":"Problem","layerKey":"intent","guidance":"State the user or system problem and explain why it is important now.","doneCriteria":["The affected user or system is clear.","The current failure or limitation is measurable."],"required":true,"allowNa":false},{"key":"goals","title":"Goals","layerKey":"intent","guidance":"State the outcomes that this design must produce and the outcomes that it does not cover.","doneCriteria":["Goals describe outcomes.","Non-goals make the scope boundary clear."],"required":true,"allowNa":false},{"key":"requirements","title":"Requirements","layerKey":"intent","guidance":"List the functional and quality requirements that constrain the design.","doneCriteria":["Each requirement is testable.","Quality requirements have explicit limits."],"required":true,"allowNa":false},{"key":"behavior","title":"Behavior","layerKey":"contract","guidance":"Describe the user-visible and system-visible behavior, including important state changes.","doneCriteria":["The main flow is complete.","Important error and recovery flows are present."],"required":true,"allowNa":false},{"key":"api","title":"API","layerKey":"contract","guidance":"Define the interfaces that callers use, including request, response, and compatibility rules.","doneCriteria":["Inputs and outputs are explicit.","Errors and compatibility rules are explicit."],"required":false,"allowNa":true},{"key":"alternatives","title":"Alternatives","layerKey":"system","guidance":"Compare the serious alternatives and state why the selected direction is better.","doneCriteria":["At least one credible alternative is assessed.","Trade-offs are explicit."],"required":true,"allowNa":false},{"key":"design","title":"Design","layerKey":"system","guidance":"Explain the selected design, its main components, and the important control flow.","doneCriteria":["Responsibilities and control flow are clear.","The design satisfies the requirements."],"required":true,"allowNa":false},{"key":"data-model","title":"Data model","layerKey":"system","guidance":"Define stored entities, ownership, lifecycle, and consistency rules.","doneCriteria":["Stored fields and relationships are clear.","Lifecycle and consistency rules are clear."],"required":false,"allowNa":true},{"key":"interfaces","title":"Interfaces","layerKey":"system","guidance":"Define internal boundaries, dependencies, and the data that crosses each boundary.","doneCriteria":["Each boundary has an owner.","Data and failure behavior are defined."],"required":false,"allowNa":true},{"key":"failure-modes","title":"Failure modes","layerKey":"system","guidance":"Describe expected failures, detection, recovery, and data safety.","doneCriteria":["Important failures have detection and recovery paths.","Durability risks are explicit."],"required":true,"allowNa":false},{"key":"rollout","title":"Rollout","layerKey":"system","guidance":"Describe deployment, migration, observation, and rollback.","doneCriteria":["The rollout has verification points.","Rollback or forward recovery is defined."],"required":true,"allowNa":false}]'::jsonb,
  '{"alternatives":"on","talkItThrough":"suggested","gapCheck":"on"}'::jsonb
);
