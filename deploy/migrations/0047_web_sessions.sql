-- Human login sessions (ADR 0031).
--
-- Each row backs one HttpOnly+Secure+SameSite=Lax cookie. The cookie
-- carries an opaque random token; only its SHA-256 hash is stored here, so
-- a read of this table can't mint a valid cookie. DB-backed (rather than a
-- stateless browser-held JWT) so logout and deprovisioning are an instant
-- row delete with no token-expiry replay window.
--
-- Machine callers (host-agent, CLI) keep using the deployment bearer token
-- and never get a row here — this table is humans only.
--
-- ON DELETE CASCADE on user_id: deprovisioning a user (or `DELETE FROM
-- users`) drops all their live sessions too.
CREATE TABLE IF NOT EXISTS web_sessions (
    token_hash   BYTEA       PRIMARY KEY,
    user_id      UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS web_sessions_user_idx ON web_sessions (user_id);
-- The expired-row sweep filters on expires_at.
CREATE INDEX IF NOT EXISTS web_sessions_expires_idx ON web_sessions (expires_at);
