-- ADR 0122: extend the registry-credentials CHECK constraint to allow
-- `aws_ecr` as an auth_kind. ECR pulls exchange the runtime's ambient
-- AWS IAM identity (IRSA / instance role) for a ~12 h basic-auth token
-- via ecr:GetAuthorizationToken — no stored secret material, same
-- cloud-IAM shape as `gcp_workload_identity`. The variant payload
-- (JSONB `auth_config`) carries an optional `assume_role_arn` for the
-- designed-in cross-account path.
--
-- New file rather than an edit to 0013: applied migrations are
-- checksum-immutable (sqlx embeds checksums; the coordinator crashes
-- on boot on a mismatch).

ALTER TABLE registry_credentials
    DROP CONSTRAINT IF EXISTS registry_credentials_auth_kind_check;

ALTER TABLE registry_credentials
    ADD CONSTRAINT registry_credentials_auth_kind_check
    CHECK (auth_kind IN ('static', 'gcp_workload_identity', 'aws_ecr', 'anonymous'));
