-- Spec mode v2 uses action-oriented section states and removes frontier-only
-- metadata, so persisted states and transcript actions must use the new contract.
ALTER TABLE "spec_section_state" RENAME COLUMN "confirmed_by" TO "settled_by";--> statement-breakpoint
ALTER TABLE "spec_section_state" RENAME CONSTRAINT "spec_section_state_confirmed_by_user_id_fk" TO "spec_section_state_settled_by_user_id_fk";--> statement-breakpoint
UPDATE "spec_section_state"
SET "state" = CASE "state"
	WHEN 'empty' THEN 'open'
	WHEN 'drafted' THEN 'proposed'
	WHEN 'confirmed' THEN 'settled'
	ELSE "state"
END;--> statement-breakpoint
ALTER TABLE "spec_section_state" ADD CONSTRAINT "spec_section_state_state_check" CHECK ("state" IN ('open', 'proposed', 'settled', 'n/a'));--> statement-breakpoint
UPDATE "spec_transcript_action"
SET "chip" = jsonb_set(
	jsonb_set(
		jsonb_set(
			jsonb_set(
				"chip" - 'provisional',
				'{before,state}',
				to_jsonb(CASE "chip" #>> '{before,state}'
					WHEN 'empty' THEN 'open'
					WHEN 'drafted' THEN 'proposed'
					WHEN 'confirmed' THEN 'settled'
					ELSE "chip" #>> '{before,state}'
				END),
				false
			),
			'{after,state}',
			to_jsonb(CASE "chip" #>> '{after,state}'
				WHEN 'empty' THEN 'open'
				WHEN 'drafted' THEN 'proposed'
				WHEN 'confirmed' THEN 'settled'
				ELSE "chip" #>> '{after,state}'
			END),
			false
		),
		'{undo,expected,state}',
		to_jsonb(CASE "chip" #>> '{undo,expected,state}'
			WHEN 'empty' THEN 'open'
			WHEN 'drafted' THEN 'proposed'
			WHEN 'confirmed' THEN 'settled'
			ELSE "chip" #>> '{undo,expected,state}'
		END),
		false
	),
	'{undo,restore,state}',
	to_jsonb(CASE "chip" #>> '{undo,restore,state}'
		WHEN 'empty' THEN 'open'
		WHEN 'drafted' THEN 'proposed'
		WHEN 'confirmed' THEN 'settled'
		ELSE "chip" #>> '{undo,restore,state}'
	END),
	false
)
WHERE "chip" ->> 'kind' = 'spec_section_state_changed';
