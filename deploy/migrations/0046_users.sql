-- User identity (ADR 0031).
--
-- engrams had no per-user identity before this — the coordinator only
-- authenticated requests against a deployment-wide bearer allowlist. This
-- table is the join point between authentication (OIDC / IAP, keyed on
-- email) and provisioning (JIT login now; SCIM 2.0 later).
--
-- JIT: on first successful login the row is upserted from the verified
-- email; the role defaults from a config bootstrap-admin allowlist, else
-- 'member'. A returning login never rewrites the role, so a manual
-- promotion/demotion survives.
--
-- SCIM-ready columns (not driven yet): `role_source` lets SCIM-later be
-- authoritative-with-manual-override — SCIM group sync only overwrites
-- 'scim'/'claim'-sourced roles, never 'manual'. `active` is the
-- deprovisioning gate (an inactive user is rejected even if the edge proxy
-- still authenticates them). `groups` is the SCIM group membership, empty
-- until SCIM lands.
CREATE TABLE IF NOT EXISTS users (
    id           UUID        PRIMARY KEY,
    email        TEXT        NOT NULL UNIQUE,
    display_name TEXT,
    role         TEXT        NOT NULL DEFAULT 'member' CHECK (role IN ('admin', 'member')),
    role_source  TEXT        NOT NULL DEFAULT 'claim'  CHECK (role_source IN ('manual', 'scim', 'claim')),
    active       BOOLEAN     NOT NULL DEFAULT TRUE,
    groups       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ
);

-- email is the lookup key on every login (JIT upsert) and the SCIM-later
-- reconciliation key. UNIQUE already builds an index, but name it for clarity.
CREATE INDEX IF NOT EXISTS users_email_idx ON users (email);
