ALTER TABLE "profile" ADD COLUMN "designation" text;--> statement-breakpoint
CREATE UNIQUE INDEX "profile_designation_unique" ON "profile" USING btree ("designation") WHERE designation is not null;