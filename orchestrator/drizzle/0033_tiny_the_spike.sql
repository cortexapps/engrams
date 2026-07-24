CREATE TABLE "dbos_sweep_lease" (
	"name" text PRIMARY KEY NOT NULL,
	"owner" text NOT NULL,
	"expires_at" timestamp with time zone NOT NULL
);
--> statement-breakpoint
CREATE TABLE "dbos_sweep_ledger" (
	"workflow_uuid" text PRIMARY KEY NOT NULL,
	"workflow_name" text NOT NULL,
	"sweep_count" integer DEFAULT 0 NOT NULL,
	"first_swept_at" timestamp with time zone,
	"last_swept_at" timestamp with time zone,
	"suppressed" boolean DEFAULT false NOT NULL,
	"cleanup_done_at" timestamp with time zone,
	"cleanup_fn" text,
	"alerted_at" timestamp with time zone
);
--> statement-breakpoint
CREATE TABLE "dbos_sweep_state" (
	"key" text PRIMARY KEY NOT NULL,
	"epoch_ms" bigint NOT NULL
);
--> statement-breakpoint
CREATE TABLE "dbos_version_heartbeats" (
	"application_version" text NOT NULL,
	"pod_name" text NOT NULL,
	"last_seen" timestamp with time zone NOT NULL,
	CONSTRAINT "dbos_version_heartbeats_application_version_pod_name_pk" PRIMARY KEY("application_version","pod_name")
);
