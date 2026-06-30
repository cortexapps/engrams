-- ADR 0062: the harness catalog — the registry of selectable agent harnesses.
--
-- A harness is chosen per session (not baked into the image). Every harness —
-- built-in or custom — is an OCI artifact registered here; the coordinator pulls
-- it, extracts its tree, validates its `harness.toml`, stores the row, and
-- re-packs the single content-addressed "catalog squashfs" holding every live
-- harness under `<name>/`. That catalog generation mounts on `dyn_0` and the
-- session `exec`s `/opt/engram/dyn/0/<name>/<exec>` (selection is the argv
-- subtree; the drive is identical across sessions, staged once per host).
--
-- `harness_catalog` holds one row per registered harness; `tree_sha256` is the
-- content address of the harness's extracted-tree tarball (blob key
-- `harness-trees/sha256/<tree_sha256>`), materialized to re-pack the catalog on
-- any registration change. `descriptor_toml` is the harness's `harness.toml`
-- (ADR 0063) — its env contract drives the orchestrator/web pickers and its
-- launch contract (exec/args) drives argv. `owner` is attribution / GC ownership
-- only (the catalog is org-shared), mirroring `mount_catalog`.

CREATE TABLE harness_catalog (
    id              TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    owner           TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    oci_ref         TEXT        NOT NULL,
    manifest_digest TEXT        NOT NULL,
    descriptor_toml TEXT        NOT NULL,
    tree_sha256     TEXT        NOT NULL,
    tree_size_bytes BIGINT      NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ
);

-- One live row per name (a soft-delete frees the name); also the
-- `ON CONFLICT (name) WHERE deleted_at IS NULL` upsert target.
CREATE UNIQUE INDEX harness_catalog_live_name ON harness_catalog (name) WHERE deleted_at IS NULL;

-- The tree-blob pin scan + re-pack materialize hit live rows by content.
CREATE INDEX harness_catalog_live_tree ON harness_catalog (tree_sha256) WHERE deleted_at IS NULL;

-- The current packed catalog generation (the squashfs that mounts on dyn_0). A
-- singleton row (id = TRUE): re-packed + upserted on every registration change.
-- `bundle_pin_set()` pins this sha so a fresh session can always mount it, and
-- the host materializes it via the existing materialize-by-sha bundle supervisor
-- (blob key `bundles/sha256/<sha256>`, staged at /var/lib/engram/shared/).
CREATE TABLE harness_catalog_generation (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    sha256      TEXT        NOT NULL,
    size_bytes  BIGINT      NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
