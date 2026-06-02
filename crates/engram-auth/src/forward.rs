//! `ForwardAuthVerifier` — trusts a signed JWT forwarded by an upstream edge
//! proxy. The proxy already authenticated the user (so no double login behind
//! it); we cryptographically verify its assertion rather than trusting a
//! plaintext header. Parameterized entirely by [`ForwardAuthConfig`] — GCP
//! IAP is one preset of those fields, not a code path, so the same verifier
//! works behind Cloudflare Access / oauth2-proxy / etc.

use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;

use crate::config::ForwardAuthConfig;
use crate::error::AuthError;
use crate::jwks::{fetch_jwks, resolve_key, validate_claims};
use crate::verify::{IdentityVerifier, Verified, VerifyInput};

pub struct ForwardAuthVerifier {
    cfg: ForwardAuthConfig,
    http: reqwest::Client,
    /// JWKS cache; refreshed on a `kid` miss (key rotation).
    cache: RwLock<Option<JwkSet>>,
}

impl ForwardAuthVerifier {
    pub fn new(cfg: ForwardAuthConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            cfg,
            http,
            cache: RwLock::new(None),
        }
    }

    /// Cached JWKS, fetching on first use. The lock is never held across the
    /// await — we clone the cached value out or fetch lock-free.
    async fn cached_jwks(&self) -> Result<JwkSet, AuthError> {
        if let Some(set) = self.cache.read().unwrap().clone() {
            return Ok(set);
        }
        let set = fetch_jwks(&self.http, &self.cfg.jwks_url).await?;
        *self.cache.write().unwrap() = Some(set.clone());
        Ok(set)
    }

    async fn refresh_jwks(&self) -> Result<JwkSet, AuthError> {
        let set = fetch_jwks(&self.http, &self.cfg.jwks_url).await?;
        *self.cache.write().unwrap() = Some(set.clone());
        Ok(set)
    }
}

#[async_trait]
impl IdentityVerifier for ForwardAuthVerifier {
    async fn verify(&self, input: &VerifyInput) -> Result<Option<Verified>, AuthError> {
        let Some(assertion) = input.header(&self.cfg.header) else {
            // No assertion forwarded → this verifier doesn't apply.
            return Ok(None);
        };

        // Resolve the signing key, refreshing the JWKS once on a kid miss
        // (the proxy rotated keys since our last fetch).
        let jwks = self.cached_jwks().await?;
        let (key, alg) = match resolve_key(&jwks, assertion) {
            Ok(k) => k,
            Err(_) => {
                let jwks = self.refresh_jwks().await?;
                resolve_key(&jwks, assertion)?
            }
        };

        // A present-but-invalid assertion is a hard failure (Err, not
        // fall-through): behind a proxy this is the authoritative credential.
        let verified = validate_claims(
            assertion,
            &key,
            alg,
            self.cfg.issuer.as_deref(),
            self.cfg.audience.as_deref(),
            &self.cfg.email_claim,
            None,
        )?;
        Ok(Some(Verified::Email(verified)))
    }

    fn name(&self) -> &'static str {
        "forward-auth"
    }
}
