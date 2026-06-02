//! The [`IdentityVerifier`] trait and its inputs/outputs.

use std::collections::HashMap;

use async_trait::async_trait;
use engram_core::types::user::Principal;

use crate::error::AuthError;

/// Framework-free view of the request a verifier inspects. The coordinator
/// builds this from the axum request (lowercased header names, parsed
/// cookies) so this crate never depends on axum.
#[derive(Clone, Debug, Default)]
pub struct VerifyInput {
    /// Header name (lowercased) → value (first occurrence).
    pub headers: HashMap<String, String>,
    /// Cookie name → value.
    pub cookies: HashMap<String, String>,
}

impl VerifyInput {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    /// The bearer token from `Authorization: Bearer <token>`, if present.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
    }
}

/// A verified email + optional display name, carried by mechanisms that
/// authenticate by identity claim (forward-auth, OIDC). The chain
/// JIT-upserts it into a [`Principal`].
#[derive(Clone, Debug)]
pub struct VerifiedEmail {
    pub email: String,
    pub display_name: Option<String>,
}

/// What a verifier produces on success: either a fully-resolved principal
/// (cookie session, service bearer, synthetic admin) or a verified email
/// that the chain resolves via JIT upsert.
#[derive(Clone, Debug)]
pub enum Verified {
    Principal(Principal),
    Email(VerifiedEmail),
}

/// One authentication mechanism. Implementations are held as
/// `Box<dyn IdentityVerifier>` in a [`VerifierChain`](crate::VerifierChain)
/// and tried in order.
#[async_trait]
pub trait IdentityVerifier: Send + Sync {
    /// `Ok(Some(_))` — this verifier handled the request.
    /// `Ok(None)` — no credential for this verifier; try the next.
    /// `Err(_)` — a credential was present but invalid (reject; do not fall
    /// through to a weaker verifier).
    async fn verify(&self, input: &VerifyInput) -> Result<Option<Verified>, AuthError>;

    /// Stable name for logging/metrics.
    fn name(&self) -> &'static str;
}
