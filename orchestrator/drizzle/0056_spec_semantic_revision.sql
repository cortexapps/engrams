ALTER TABLE "spec" ADD COLUMN "current_semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec" SET "current_semantic_doc_seq" = "current_doc_seq";--> statement-breakpoint
ALTER TABLE "spec" ALTER COLUMN "current_semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec" ALTER COLUMN "current_semantic_doc_seq" SET DEFAULT 0;--> statement-breakpoint
ALTER TABLE "spec_projection" ADD COLUMN "semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_projection" SET "semantic_doc_seq" = "doc_seq";--> statement-breakpoint
ALTER TABLE "spec_projection" ALTER COLUMN "semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_snapshot" ADD COLUMN "covered_semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_snapshot" SET "covered_semantic_doc_seq" = "covered_seq";--> statement-breakpoint
ALTER TABLE "spec_snapshot" ALTER COLUMN "covered_semantic_doc_seq" SET NOT NULL;--> statement-breakpoint
ALTER TABLE "spec_update_log" ADD COLUMN "semantic_doc_seq" bigint;--> statement-breakpoint
UPDATE "spec_update_log" SET "semantic_doc_seq" = "seq";--> statement-breakpoint
ALTER TABLE "spec_update_log" ALTER COLUMN "semantic_doc_seq" SET NOT NULL;
