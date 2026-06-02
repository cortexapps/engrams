//! The per-request verifier chain. Tries each [`IdentityVerifier`] in order;
//! the first that handles the request wins. Verifiers that carry a verified
//! email (forward-auth) are JIT-upserted into a user row here, applying the
//! bootstrap-admin allowlist.

use std::sync::Arc;

use engram_core::traits::UserStore;
use engram_core::types::user::{Principal, Role, RoleSource};

use crate::error::AuthError;
use crate::verify::{IdentityVerifier, Verified, VerifiedEmail, VerifyInput};

pub struct VerifierChain {
    verifiers: Vec<Box<dyn IdentityVerifier>>,
    users: Arc<dyn UserStore>,
    /// Lowercased emails promoted to admin on JIT upsert.
    bootstrap_admins: Vec<String>,
}

impl VerifierChain {
    pub fn new(
        verifiers: Vec<Box<dyn IdentityVerifier>>,
        users: Arc<dyn UserStore>,
        bootstrap_admins: Vec<String>,
    ) -> Self {
        let bootstrap_admins = bootstrap_admins
            .into_iter()
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        Self {
            verifiers,
            users,
            bootstrap_admins,
        }
    }

    /// Resolve the request to a `Principal`, or `Ok(None)` if no verifier
    /// authenticated it (→ the caller returns 401). `Err` means a credential
    /// was present but invalid, or a deprovisioned user tried to ride a
    /// session.
    pub async fn resolve(&self, input: &VerifyInput) -> Result<Option<Principal>, AuthError> {
        for v in &self.verifiers {
            match v.verify(input).await {
                Ok(Some(Verified::Principal(p))) => {
                    if !p.active {
                        return Err(AuthError::Inactive);
                    }
                    return Ok(Some(p));
                }
                Ok(Some(Verified::Email(e))) => {
                    return Ok(Some(self.jit_upsert(e).await?));
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// JIT-provision a verified email into a principal — public so the OIDC
    /// `/auth/callback` handler resolves its email through the same path as
    /// the chain's forward-auth/synthetic verifiers.
    pub async fn provision(&self, e: VerifiedEmail) -> Result<Principal, AuthError> {
        self.jit_upsert(e).await
    }

    /// JIT-provision a user from a verified email and project to a principal.
    /// New bootstrap-admin emails are created as admin; an already-present
    /// allowlisted email that isn't admin (and wasn't *manually* set) is
    /// promoted — so adding someone to the allowlist after their first login
    /// still grants admin. A manual role is never overridden.
    async fn jit_upsert(&self, e: VerifiedEmail) -> Result<Principal, AuthError> {
        let is_bootstrap = self
            .bootstrap_admins
            .iter()
            .any(|a| *a == e.email.to_ascii_lowercase());
        let default_role = if is_bootstrap {
            Role::Admin
        } else {
            Role::Member
        };
        let user = self
            .users
            .upsert_user_by_email(
                &e.email,
                e.display_name.as_deref(),
                default_role,
                RoleSource::Claim,
            )
            .await?;
        let user =
            if is_bootstrap && user.role != Role::Admin && user.role_source != RoleSource::Manual {
                self.users
                    .set_user_role(user.id, Role::Admin, RoleSource::Claim)
                    .await?
            } else {
                user
            };
        if !user.active {
            return Err(AuthError::Inactive);
        }
        Ok(user.to_principal())
    }
}
