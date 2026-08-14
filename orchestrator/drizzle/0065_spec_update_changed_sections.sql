-- The section-scoped write fence (ADR 0114 amendment): each update row names
-- the sections it changed, so an agent mutation with expected_rev is rejected
-- only when its TARGET section changed past that revision — not whenever any
-- human typed anywhere in the document.
ALTER TABLE "spec_update_log" ADD COLUMN "changed_section_ids" text[] DEFAULT '{}'::text[] NOT NULL;
