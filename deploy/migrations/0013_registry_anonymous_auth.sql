-- Stage C: extend the registry-credentials CHECK constraint to allow
-- `anonymous` as an auth_kind. Public registries (Docker Hub public,
-- ghcr.io public, the local dev `registry:2`) need a row in
-- `registry_credentials` so the dashboard can list them and an
-- operator can later upgrade in place to `static` /
-- `gcp_workload_identity`, but they carry no secret material — just
-- the host name. The matching trait dispatch in
-- `engram-oci-auth::PgAuthResolver` returns `None` for this kind, so
-- the OCI client falls through to anonymous pull as before.

ALTER TABLE registry_credentials
    DROP CONSTRAINT IF EXISTS registry_credentials_auth_kind_check;

ALTER TABLE registry_credentials
    ADD CONSTRAINT registry_credentials_auth_kind_check
    CHECK (auth_kind IN ('static', 'gcp_workload_identity', 'anonymous'));
