CREATE TABLE "spec_chat_message" (
  "prompt_id"      text PRIMARY KEY,
  "spec_id"        uuid NOT NULL REFERENCES "spec"("id") ON DELETE cascade,
  "author_user_id" text REFERENCES "user"("id") ON DELETE set null,
  "author_name"    text NOT NULL,
  "text"           text NOT NULL,
  "created_at"     timestamptz NOT NULL DEFAULT now()
);--> statement-breakpoint
CREATE INDEX "spec_chat_message_spec_created_idx"
  ON "spec_chat_message" ("spec_id", "created_at");
