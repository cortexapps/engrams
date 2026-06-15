//! The per-request verifier chain. Tries each [`IdentityVerifier`] in order;
//! the first that handles the request wins.

use engram_core::types::user::Principal;

use crate::error::AuthError;
use crate::verify::{IdentityVerifier, Verified, VerifyInput};

pub struct VerifierChain {
    verifiers: Vec<Box<dyn IdentityVerifier>>,
}

impl VerifierChain {
    pub fn new(verifiers: Vec<Box<dyn IdentityVerifier>>) -> Self {
        Self { verifiers }
    }

    /// Resolve the request to a `Principal`, or `Ok(None)` if no verifier
    /// authenticated it (→ the caller returns 401). `Err` means a credential
    /// was present but invalid.
    pub async fn resolve(&self, input: &VerifyInput) -> Result<Option<Principal>, AuthError> {
        for v in &self.verifiers {
            match v.verify(input).await {
                Ok(Some(Verified::Principal(p))) => {
                    if !p.active {
                        return Err(AuthError::Inactive);
                    }
                    return Ok(Some(p));
                }
                Ok(Some(Verified::Email(_))) => {
                    // Email-based verification requires JIT upsert (users table).
                    // That table is dropped in ADR 0039 Task 31; skip.
                    continue;
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Kept for OIDC callback compat path — returns Err since users table is gone.
    pub async fn provision(
        &self,
        _e: crate::verify::VerifiedEmail,
    ) -> Result<Principal, AuthError> {
        Err(AuthError::Config(
            "provision: users table removed in ADR 0039 Task 31".into(),
        ))
    }
}
