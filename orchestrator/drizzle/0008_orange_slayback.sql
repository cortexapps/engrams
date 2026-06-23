CREATE TABLE "connector_logo" (
	"provider" text PRIMARY KEY NOT NULL,
	"media_type" text NOT NULL,
	"data" "bytea" NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL
);
