-- Spec mode no longer has template-controlled stages.
ALTER TABLE "spec_template" DROP COLUMN "stage_flags";
