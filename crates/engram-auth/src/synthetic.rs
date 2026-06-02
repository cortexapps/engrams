//! `SyntheticAdmin` — the no-SSO-configured fallback. Returns one built-in
//! admin principal for every request so `just dev` runs with zero auth setup
//! while still exercising the real authed code paths.

use async_trait::async_trait;
use engram_core::types::user::{Principal, Role};

use crate::error::AuthError;
use crate::verify::{IdentityVerifier, Verified, VerifyInput};
use crate::SYNTHETIC_USER_ID;

pub struct SyntheticAdmin {
    principal: Principal,
}

impl SyntheticAdmin {
    /// `email` is the dev committer/display identity (configurable via
    /// `--dev-default-email`).
    pub fn new(email: impl Into<String>) -> Self {
        let email = email.into();
        Self {
            principal: Principal {
                user_id: SYNTHETIC_USER_ID.into(),
                display_name: Some("Local Admin".to_string()),
                email,
                role: Role::Admin,
                active: true,
            },
        }
    }
}

#[async_trait]
impl IdentityVerifier for SyntheticAdmin {
    async fn verify(&self, _input: &VerifyInput) -> Result<Option<Verified>, AuthError> {
        Ok(Some(Verified::Principal(self.principal.clone())))
    }

    fn name(&self) -> &'static str {
        "synthetic-admin"
    }
}
