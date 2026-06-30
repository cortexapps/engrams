-- ADR 0062 (per-harness-bundle model): a registered (custom) harness is now its
-- own content-addressed RO squashfs, mounted on dyn_0 and materialized like an
-- uploaded skill — there is no packed "catalog generation" drive. Built-in
-- harnesses (claude) ride the host-image current_bundles stamp + the
-- coordinator's embedded descriptor and never have a row here.
--
-- So: the catalog row stores the harness's own squashfs sha (not an extracted-tree
-- tarball hash), and the singleton generation table is gone. 0074 is freshly
-- shipped with no live custom rows, so renaming the columns loses nothing.

ALTER TABLE harness_catalog RENAME COLUMN tree_sha256 TO squashfs_sha256;
ALTER TABLE harness_catalog RENAME COLUMN tree_size_bytes TO squashfs_size_bytes;

-- The partial index on the content hash follows the rename; give it a name that
-- matches the new column.
ALTER INDEX harness_catalog_live_tree RENAME TO harness_catalog_live_squashfs;

-- The packed-catalog "current generation" singleton is retired entirely.
DROP TABLE harness_catalog_generation;
