//! Dev/test [`Integration`] implementations.
//!
//! [`StaticGitHubIntegration`] mints a fixed credential and records every
//! action it's asked to perform, so coordinator/integration tests can drive
//! the ADR 0056 integration seam end-to-end without a live provider. Never use
//! in production — it authenticates nothing.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use engram_core::error::IntegrationError;
use engram_core::traits::{CredentialHint, Integration, ScopedCredential};
use engram_core::types::Capability;
use parking_lot::Mutex;

/// An `Integration` that mints a fixed credential and records every
/// `perform_action` (pull-request) call in order.
pub struct StaticGitHubIntegration {
    token: String,
    /// The recorded `perform_action` args, in order (the PR specs).
    pull_requests: Mutex<Vec<serde_json::Value>>,
    /// ADR 0056: the capability sets each `mint_credential` was asked for, in
    /// order — so a test can assert the bound caps reached the mint.
    minted_caps: Mutex<Vec<Vec<Capability>>>,
    next_id: AtomicU64,
}

impl StaticGitHubIntegration {
    /// Mock integration minting `token` as the git password.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            pull_requests: Mutex::new(Vec::new()),
            minted_caps: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The common test shape — a `github` mock with the given token.
    pub fn github(token: impl Into<String>) -> Self {
        Self::new(token)
    }

    /// Snapshot of every `perform_action` (PR) call's args, in order.
    pub fn recorded_pull_requests(&self) -> Vec<serde_json::Value> {
        self.pull_requests.lock().clone()
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

    async fn perform_action(
        &self,
        cap: &Capability,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, IntegrationError> {
        if cap.action != "pulls:write" {
            return Err(IntegrationError::Unsupported);
        }
        let repo = args
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IntegrationError::InvalidSpec("missing repo".into()))?
            .to_string();
        self.pull_requests.lock().push(args.clone());
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({
            "url": format!("https://github.com/{repo}/pull/{id}"),
            "id": id,
            "number": id,
            "state": "open",
        }))
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

    #[tokio::test]
    async fn records_pull_requests_with_incrementing_ids() {
        let it = StaticGitHubIntegration::github("ghs_test");
        let cap = Capability::parse("github:pulls:write").unwrap();
        let args = serde_json::json!({
            "repo": "cortexapps/engrams",
            "head_branch": "feat/x",
            "base_branch": "main",
            "title": "Add x",
        });
        let pr1 = it.perform_action(&cap, &args).await.unwrap();
        let pr2 = it.perform_action(&cap, &args).await.unwrap();
        assert_eq!(pr1["id"], 1);
        assert_eq!(pr2["id"], 2);
        assert_eq!(pr1["url"], "https://github.com/cortexapps/engrams/pull/1");
        assert_eq!(it.recorded_pull_requests().len(), 2);
        assert_eq!(it.recorded_pull_requests()[0]["title"], "Add x");
    }

    #[tokio::test]
    async fn rejects_non_pull_request_actions() {
        let it = StaticGitHubIntegration::github("ghs_test");
        let cap = Capability::parse("github:contents:write").unwrap();
        let err = it
            .perform_action(&cap, &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, IntegrationError::Unsupported));
    }
}
