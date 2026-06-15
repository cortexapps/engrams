CREATE TABLE "user_session_secrets" (
	"user_id" text NOT NULL,
	"env_var_name" text NOT NULL,
	"wrapped_dek" text NOT NULL,
	"nonce" text NOT NULL,
	"ciphertext" text NOT NULL,
	"key_id" text NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL,
	CONSTRAINT "user_session_secrets_user_id_env_var_name_pk" PRIMARY KEY("user_id","env_var_name")
);
--> statement-breakpoint
ALTER TABLE "user_session_secrets" ADD CONSTRAINT "user_session_secrets_user_id_user_id_fk" FOREIGN KEY ("user_id") REFERENCES "public"."user"("id") ON DELETE cascade ON UPDATE no action;