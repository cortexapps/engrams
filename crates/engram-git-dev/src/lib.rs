//! Dev/test [`GitForge`] implementations.
//!
//! [`StaticGitForge`] mints a fixed credential and records every change
//! request it's asked to open, so coordinator/integration tests can
//! drive the ADR 0023 forge seam end-to-end without a live provider.
//! Never use in production — it authenticates nothing.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use engram_core::error::GitForgeError;
use engram_core::traits::{
    ForgeKind, GitForge, PullRequest, PullRequestSpec, RepoRef, ScopedToken,
};
use engram_core::types::Capability;
use parking_lot::Mutex;

/// A `GitForge` that mints a fixed token and records `create_pull_request`
/// calls in order.
pub struct StaticGitForge {
    host: String,
    token: String,
    kind: ForgeKind,
    pull_requests: Mutex<Vec<(RepoRef, PullRequestSpec)>>,
    /// ADR 0056: the capability sets each `mint_installation_token` was asked
    /// for, in order — so a test can assert the bound caps reached the mint.
    minted_caps: Mutex<Vec<Vec<Capability>>>,
    next_id: AtomicU64,
}

impl StaticGitForge {
    /// Mock forge on `host` minting `token` as the git password.
    pub fn new(host: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            token: token.into(),
            kind: ForgeKind::GitHub,
            pull_requests: Mutex::new(Vec::new()),
            minted_caps: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// `github.com` mock with the given token — the common test shape.
    pub fn github(token: impl Into<String>) -> Self {
        Self::new("github.com", token)
    }

    /// Snapshot of every `create_pull_request` call, in order.
    pub fn recorded_pull_requests(&self) -> Vec<(RepoRef, PullRequestSpec)> {
        self.pull_requests.lock().clone()
    }

    /// Snapshot of the capability set passed to each `mint_installation_token`.
    pub fn recorded_mint_caps(&self) -> Vec<Vec<Capability>> {
        self.minted_caps.lock().clone()
    }
}

#[async_trait]
impl GitForge for StaticGitForge {
    async fn mint_installation_token(
        &self,
        caps: &[Capability],
        _owner: Option<&str>,
    ) -> Result<ScopedToken, GitForgeError> {
        self.minted_caps.lock().push(caps.to_vec());
        Ok(ScopedToken {
            username: "x-access-token".to_string(),
            password: self.token.clone(),
            expires_at: Utc::now() + Duration::hours(1),
        })
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        pr: &PullRequestSpec,
    ) -> Result<PullRequest, GitForgeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.pull_requests.lock().push((repo.clone(), pr.clone()));
        Ok(PullRequest {
            url: format!("https://{}/{}/pull/{}", self.host, repo, id),
            id,
            state: "open".to_string(),
        })
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn kind(&self) -> ForgeKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mints_the_configured_token() {
        let forge = StaticGitForge::github("ghs_test");
        let tok = forge.mint_installation_token(&[], None).await.unwrap();
        assert_eq!(tok.username, "x-access-token");
        assert_eq!(tok.password, "ghs_test");
        assert!(tok.expires_at > Utc::now());
    }

    #[tokio::test]
    async fn records_the_caps_it_was_minted_for() {
        let forge = StaticGitForge::github("ghs_test");
        let caps = vec![Capability::parse("github:contents:read").unwrap()];
        forge.mint_installation_token(&caps, None).await.unwrap();
        let recorded = forge.recorded_mint_caps();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0][0].action, "contents:read");
    }

    #[tokio::test]
    async fn records_pull_requests_with_incrementing_ids() {
        let forge = StaticGitForge::github("ghs_test");
        let repo = RepoRef::parse("cortexapps/engrams").unwrap();
        let spec = PullRequestSpec {
            head_branch: "feat/x".into(),
            base_branch: "main".into(),
            title: "Add x".into(),
            body: "body".into(),
            draft: false,
        };
        let pr1 = forge.create_pull_request(&repo, &spec).await.unwrap();
        let pr2 = forge.create_pull_request(&repo, &spec).await.unwrap();
        assert_eq!(pr1.id, 1);
        assert_eq!(pr2.id, 2);
        assert_eq!(pr1.url, "https://github.com/cortexapps/engrams/pull/1");
        assert_eq!(forge.recorded_pull_requests().len(), 2);
        assert_eq!(forge.recorded_pull_requests()[0].1.title, "Add x");
    }
}
