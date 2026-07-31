CREATE TABLE "integration_oidc_key" (
	"kid" text PRIMARY KEY NOT NULL,
	"public_jwk" jsonb NOT NULL,
	"wrapped_dek" "bytea" NOT NULL,
	"nonce" "bytea" NOT NULL,
	"ciphertext" "bytea" NOT NULL,
	"key_id" text NOT NULL,
	"state" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"publish_until" timestamp with time zone
);
--> statement-breakpoint
CREATE UNIQUE INDEX "integration_oidc_key_active_unique" ON "integration_oidc_key" USING btree ("state") WHERE state = 'active';