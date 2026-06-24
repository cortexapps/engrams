//! Dev/test [`Integration`] implementations.
//!
//! [`StaticGitHubIntegration`] mints a fixed credential and records the
//! capability sets it's asked to mint for, so coordinator/integration tests
//! can drive the ADR 0056 integration seam end-to-end without a live provider.
//! Never use in production — it authenticates nothing.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use engram_core::error::IntegrationError;
use engram_core::traits::{CredentialHint, Integration, ScopedCredential};
use engram_core::types::Capability;
use parking_lot::Mutex;

/// An `Integration` that mints a fixed credential and records the capability
/// sets each mint was asked for.
pub struct StaticGitHubIntegration {
    token: String,
    /// ADR 0056: the capability sets each `mint_credential` was asked for, in
    /// order — so a test can assert the bound caps reached the mint.
    minted_caps: Mutex<Vec<Vec<Capability>>>,
}

impl StaticGitHubIntegration {
    /// Mock integration minting `token` as the git password.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            minted_caps: Mutex::new(Vec::new()),
        }
    }

    /// The common test shape — a `github` mock with the given token.
    pub fn github(token: impl Into<String>) -> Self {
        Self::new(token)
    }

    /// Snapshot of the capability set passed to each `mint_credential`.
    pub fn recorded_mint_caps(&self) -> Vec<Vec<Capability>> {
        self.minted_caps.lock().clone()
    }
}

#[async_trait]
impl Integration for StaticGitHubIntegration {
    fn provider(&self) -> &str {
        "github"
    }

    async fn mint_credential(
        &self,
        caps: &[Capability],
        _hint: &CredentialHint,
    ) -> Result<ScopedCredential, IntegrationError> {
        self.minted_caps.lock().push(caps.to_vec());
        Ok(ScopedCredential::Basic {
            username: "x-access-token".to_string(),
            password: self.token.clone(),
            expires_at: Utc::now() + Duration::hours(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mints_the_configured_token() {
        let it = StaticGitHubIntegration::github("ghs_test");
        let cred = it
            .mint_credential(&[], &CredentialHint::default())
            .await
            .unwrap();
        let ScopedCredential::Basic {
            username, password, ..
        } = &cred
        else {
            panic!("expected basic credential");
        };
        assert_eq!(username, "x-access-token");
        assert_eq!(password, "ghs_test");
    }

    #[tokio::test]
    async fn records_the_caps_it_was_minted_for() {
        let it = StaticGitHubIntegration::github("ghs_test");
        let caps = vec![Capability::parse("github:contents:read").unwrap()];
        it.mint_credential(&caps, &CredentialHint::default())
            .await
            .unwrap();
        let recorded = it.recorded_mint_caps();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0][0].action, "contents:read");
    }
}
