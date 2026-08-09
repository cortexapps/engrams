CREATE TABLE "coordination_operation" (
	"caller_session_id" text NOT NULL,
	"operation" text NOT NULL,
	"idempotency_key" text NOT NULL,
	"request_hash" text NOT NULL,
	"status" text DEFAULT 'reserved' NOT NULL,
	"reserved_task_id" text,
	"reserved_session_id" text,
	"prompt_id" text,
	"result" jsonb,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "coordination_operation_caller_session_id_operation_idempotency_key_pk" PRIMARY KEY("caller_session_id","operation","idempotency_key")
);
--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "parent_task_id" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "root_task_id" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "local_task_name" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "canonical_task_name" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "spawning_session_id" text;--> statement-breakpoint
ALTER TABLE "task" ADD COLUMN "launch_policy" jsonb;--> statement-breakpoint
CREATE INDEX "coordination_operation_reserved_session_idx" ON "coordination_operation" USING btree ("reserved_session_id");--> statement-breakpoint
CREATE INDEX "task_parent_idx" ON "task" USING btree ("parent_task_id");--> statement-breakpoint
CREATE INDEX "task_root_idx" ON "task" USING btree ("root_task_id");--> statement-breakpoint
CREATE UNIQUE INDEX "task_root_canonical_name_unique" ON "task" USING btree ("root_task_id","canonical_task_name") WHERE canonical_task_name IS NOT NULL;