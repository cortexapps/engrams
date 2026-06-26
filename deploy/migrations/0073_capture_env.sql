-- Capture-time env for an image's [warm] hook.
--
-- The [warm] hook runs inside the capture VM at base-snapshot capture and
-- may need secrets (e.g. a 1Password token for brain-backend's bootRun)
-- that the image manifest deliberately does NOT carry (ADR 0057 moved
-- secrets off the manifest into session policy). Rather than bake decrypted
-- secrets into the image layer, the admin attaches a capture-time env — a
-- list of {name, value} where value is a literal or a secret ref — to the
-- *enable action*. The coordinator resolves the refs at capture (via the
-- same SecretStore a session uses) and injects them into the warm hook's
-- exec env. Stored as refs, never resolved values.
--
-- capture_env rides the enable_jobs row (the carrier for a given capture)
-- and is persisted on enabled_images (the durable source the UI shows and
-- UpdateImage rewrites).

ALTER TABLE enabled_images
    ADD COLUMN capture_env JSONB NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE enable_jobs
    ADD COLUMN capture_env JSONB NOT NULL DEFAULT '[]'::jsonb;

COMMENT ON COLUMN enabled_images.capture_env IS
    'Capture-time env for the image''s [warm] hook: a JSON array of {name, value:{kind:literal|secret_ref, ...}}. Secret refs (never values) resolved at capture and injected into the warm hook. Set via EnableImage/UpdateImage, not the manifest.';
COMMENT ON COLUMN enable_jobs.capture_env IS
    'The capture_env applied by this enable job''s capture (carried from the triggering request, or inherited from the enabled_images row on refresh).';
