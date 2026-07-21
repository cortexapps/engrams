CREATE TABLE "consumer_cursors" (
	"session_id" text NOT NULL,
	"consumer" text NOT NULL,
	"last_idx" bigint NOT NULL,
	"updated_at" timestamp with time zone NOT NULL,
	CONSTRAINT "consumer_cursors_session_id_consumer_pk" PRIMARY KEY("session_id","consumer")
);
--> statement-breakpoint
CREATE TABLE "session_listeners" (
	"session_id" text PRIMARY KEY NOT NULL,
	"owner" text,
	"lease_expires_at" timestamp with time zone,
	"terminal_at" timestamp with time zone,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "slack_session" (
	"session_id" text PRIMARY KEY NOT NULL,
	"thread_wf_id" text NOT NULL
);
--> statement-breakpoint
INSERT INTO "session_listeners" ("session_id", "terminal_at")
SELECT ts."session_id",
	CASE WHEN t."status" IN ('open', 'working') THEN NULL ELSE now() END
FROM "task_session" ts
INNER JOIN "task" t ON t."id" = ts."task_id"
ON CONFLICT ("session_id") DO NOTHING;
