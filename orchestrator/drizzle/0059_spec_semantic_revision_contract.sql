-- Contract phase for the semantic revision expand in 0056. Every orchestrator
-- pod now writes the semantic revision columns explicitly, so the compatibility
-- triggers that filled those columns for older pods have no work left. Drop the
-- six triggers and their four functions.
--
-- The four semantic revision columns stay NOT NULL. A writer that omits a
-- semantic value is now a constraint failure, not a silent backfill. The
-- 'engrams.semantic_revision_writer' session setting had these trigger bodies as
-- its only readers, so the application no longer sets it.
DROP TRIGGER "spec_legacy_semantic_revision_compat" ON "spec";
--> statement-breakpoint
DROP TRIGGER "spec_update_log_legacy_semantic_revision_compat" ON "spec_update_log";
--> statement-breakpoint
DROP TRIGGER "spec_snapshot_legacy_semantic_revision_compat" ON "spec_snapshot";
--> statement-breakpoint
DROP TRIGGER "spec_snapshot_legacy_semantic_revision_update_compat" ON "spec_snapshot";
--> statement-breakpoint
DROP TRIGGER "spec_projection_legacy_semantic_revision_compat" ON "spec_projection";
--> statement-breakpoint
DROP TRIGGER "spec_projection_legacy_semantic_revision_update_compat" ON "spec_projection";
--> statement-breakpoint
DROP FUNCTION "spec_legacy_semantic_revision_compat"();
--> statement-breakpoint
DROP FUNCTION "spec_update_log_legacy_semantic_revision_compat"();
--> statement-breakpoint
DROP FUNCTION "spec_snapshot_legacy_semantic_revision_compat"();
--> statement-breakpoint
DROP FUNCTION "spec_projection_legacy_semantic_revision_compat"();
