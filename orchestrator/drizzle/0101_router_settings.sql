CREATE TABLE "router_settings" (
	"router_id" text PRIMARY KEY NOT NULL,
	"default_model" text,
	"title_model" text,
	"decision_model" text,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL
);
