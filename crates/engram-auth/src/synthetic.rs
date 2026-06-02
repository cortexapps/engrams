//! `SyntheticAdmin` — the no-SSO-configured fallback. Authenticates every
//! request as one built-in admin so `just dev` runs with zero auth setup
//! while still exercising the real authed code paths.
//!
//! It emits a [`VerifiedEmail`] (not a bare principal) so the chain
//! JIT-upserts a real `users` row for the dev identity — that row is what
//! lets a dev save a Claude token (FK-backed) and own sessions with a real
//! user id, exactly like a production user. `build_chain` adds the dev email
//! to the bootstrap-admin allowlist so the upsert resolves to `admin`.

use async_trait::async_trait;

use crate::error::AuthError;
use crate::verify::{IdentityVerifier, Verified, VerifiedEmail, VerifyInput};

pub struct SyntheticAdmin {
    email: String,
}

impl SyntheticAdmin {
    /// `email` is the dev committer/display identity (`--dev-default-email`).
    pub fn new(email: impl Into<String>) -> Self {
        Self { email: email.into() }
    }
}

#[async_trait]
impl IdentityVerifier for SyntheticAdmin {
    async fn verify(&self, _input: &VerifyInput) -> Result<Option<Verified>, AuthError> {
        Ok(Some(Verified::Email(VerifiedEmail {
            email: self.email.clone(),
            display_name: Some("Local Admin".to_string()),
        })))
    }

    fn name(&self) -> &'static str {
        "synthetic-admin"
    }
}
