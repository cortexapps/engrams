-- ADR 0106 addendum: refresh scheduling and terminal-failure state for
-- rotating OAuth credentials (connector subjects, first consumer: Linear).
--
-- `expires_at` duplicates the bundle's access-token expiry OUTSIDE the
-- ciphertext on purpose: the refresh scanner must find due rows without a
-- KEK unwrap per row per sweep, and the status surface derives
-- connected/expired from it. It is a timestamp, not secret material.
--
-- `refresh_claim_until` is an advisory cross-replica claim: one replica
-- refreshes a due credential at a time; a lapsed claim is retaken. The
-- version CAS on the row remains the correctness fence.
--
-- `broken_at`/`broken_reason` mark a credential whose provider terminally
-- rejected refresh (invalid_grant with the row version unchanged). The
-- sealed bundle is left untouched; a fresh authorization flow clears both.
ALTER TABLE oauth_credentials
    ADD COLUMN expires_at TIMESTAMPTZ,
    ADD COLUMN refresh_claim_until TIMESTAMPTZ,
    ADD COLUMN broken_at TIMESTAMPTZ,
    ADD COLUMN broken_reason TEXT;

CREATE INDEX oauth_credentials_refresh_due_idx
    ON oauth_credentials (expires_at)
    WHERE expires_at IS NOT NULL AND revoked_at IS NULL AND broken_at IS NULL;
