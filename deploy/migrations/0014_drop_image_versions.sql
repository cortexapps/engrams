-- Stage E: drop the legacy `image_versions` table.
--
-- Stage B1 migrated session-time image lookup to `enabled_images`
-- and `sessions.image_uri`. The image_versions table had no readers
-- after Stage B1 and no writers after Stage A (which decoupled bake
-- from Postgres). Dropping it here completes the cleanup.
--
-- Foreign-key references: none. The legacy `sessions.image_repo +
-- image_tag` columns were removed in 0012 and never carried an FK
-- onto image_versions; snapshots / events also don't reference it.
-- Drop is unconditional.

DROP TABLE IF EXISTS image_versions;
