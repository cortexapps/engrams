ALTER TABLE "spec_transcript_action" ADD COLUMN "actor_user_id" text;--> statement-breakpoint
ALTER TABLE "spec_open_question" ADD COLUMN "resolved_by" text;--> statement-breakpoint
ALTER TABLE "spec_transcript_action" ADD CONSTRAINT "spec_transcript_action_actor_user_id_user_id_fk" FOREIGN KEY ("actor_user_id") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;--> statement-breakpoint
ALTER TABLE "spec_open_question" ADD CONSTRAINT "spec_open_question_resolved_by_user_id_fk" FOREIGN KEY ("resolved_by") REFERENCES "public"."user"("id") ON DELETE set null ON UPDATE no action;
