//! Provider-agnostic git forge authority (ADR 0023).
//!
//! A [`GitForge`] is a *platform-side* authority — it lives in the
//! coordinator, holds the long-lived provider credential (e.g. a GitHub
//! App private key), and exposes two provider-agnostic operations:
//!
//! 1. [`GitForge::mint_installation_token`] — mint a short-lived
//!    installation credential, valid across every repo the install can
//!    access (not one repo). The coordinator hands this to an in-session
//!    `GIT_ASKPASS` helper on demand (ADR 0023's forge seam), so the
//!    sandbox never stores a durable secret and token expiry is
//!    invisible in-guest.
//! 2. [`GitForge::create_pull_request`] — open a *change request*,
//!    neutral over GitHub Pull Requests, GitLab Merge Requests, and
//!    Gitea/Bitbucket PRs.
//!
//! The agent still drives the git work (it runs `git push` and decides
//! when to open a change request); it just calls engrams' uniform op
//! instead of shelling out to a provider-specific `gh`/`glab`, so the
//! *forge* owns provider differences. The platform performs no git data
//! operations — no clone, fetch, or push (ADR 0005).
//!
//! Switching providers is a coordinator config flag, same as
//! [`crate::traits::SecretStore`]. GitHub ships first
//! (`engram-git-github`); GitLab/Gitea are later impls of this trait.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::GitForgeError;
use crate::types::Capability;

/// Which forge a [`GitForge`] talks to. Used for repo→provider
/// matching and for labelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Gitea,
}

/// A repository on a forge, identified by owner + name (GitHub
/// `owner/repo`, GitLab namespace/project, …). The forge impl maps
/// this onto its own API addressing (path vs project-id).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    /// Parse an `owner/name` slug (the manifest `[git] repo` form).
    /// Rejects anything that isn't exactly two non-empty segments so a
    /// malformed config fails loudly rather than minting a token for the
    /// wrong repo.
    pub fn parse(slug: &str) -> Result<Self, GitForgeError> {
        let mut parts = slug.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => Ok(Self {
                owner: owner.to_string(),
                name: name.to_string(),
            }),
            _ => Err(GitForgeError::InvalidSpec(format!(
                "repo must be `owner/name`, got `{slug}`"
            ))),
        }
    }
}

impl std::fmt::Display for RepoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A short-lived credential for git operations. For a GitHub App
/// installation token this is `("x-access-token", "ghs_…")`, valid for
/// **every repo the installation can access** (not one repo); the
/// `GIT_ASKPASS` helper presents `password` to git. `expires_at` is
/// advisory for the coordinator's own caching — the guest never sees it
/// (it fetches fresh per op).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopedToken {
    pub username: String,
    pub password: String,
    pub expires_at: DateTime<Utc>,
}

/// Provider-agnostic change-request spec. Maps onto GitHub
/// (`head`/`base`/`body`) and GitLab
/// (`source_branch`/`target_branch`/`description`) alike.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSpec {
    /// The branch carrying the changes (GitHub `head`, GitLab
    /// `source_branch`). Must already be pushed to the remote.
    pub head_branch: String,
    /// The branch to merge into (GitHub `base`, GitLab `target_branch`).
    pub base_branch: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub draft: bool,
}

/// The created change request. `id` is GitHub's `number` / GitLab's
/// `iid`; `url` is the human-facing page (`html_url` / `web_url`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub url: String,
    pub id: u64,
    pub state: String,
}

#[async_trait]
pub trait GitForge: Send + Sync {
    /// Mint a short-lived credential for the forge **installation** —
    /// valid across every repo that installation can access, not a
    /// single repo. `owner` selects the installation when the app spans
    /// several orgs/users; `None` uses the app's sole installation
    /// (impls error if that's ambiguous). Called on demand by the
    /// in-session forge seam; never injected at session create. Impls
    /// cache + lazily refresh (provider tokens are typically ~1h).
    ///
    /// ADR 0056 (Plane A): `caps` are the session's bound capabilities for
    /// this forge's provider (clamped server-side at create). The impl mints
    /// a token scoped to EXACTLY those capabilities — a `github:contents:read`
    /// profile yields a read-only token. An empty set falls back to the
    /// provider's default scopes (today's behavior), so a profile that
    /// declares no capabilities is unaffected.
    async fn mint_installation_token(
        &self,
        caps: &[Capability],
        owner: Option<&str>,
    ) -> Result<ScopedToken, GitForgeError>;

    /// Open a change request against `repo` (any repo the installation
    /// can access — the agent picks it per call). The impl maps the
    /// neutral [`PullRequestSpec`] onto the provider's API and
    /// authenticates with its own credential.
    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        pr: &PullRequestSpec,
    ) -> Result<PullRequest, GitForgeError>;

    /// The forge's git host (e.g. `github.com`). Used to match a
    /// session's repo to the configured forge and to render git
    /// credential config in-guest.
    fn host(&self) -> &str;

    /// Which forge this is.
    fn kind(&self) -> ForgeKind;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_parses_owner_name() {
        let r = RepoRef::parse("cortexapps/engrams").unwrap();
        assert_eq!(r.owner, "cortexapps");
        assert_eq!(r.name, "engrams");
        assert_eq!(r.to_string(), "cortexapps/engrams");
    }

    #[test]
    fn repo_ref_rejects_malformed_slugs() {
        for bad in ["", "noslash", "a/b/c", "/b", "a/", "/"] {
            assert!(
                RepoRef::parse(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn forge_kind_round_trips_snake_case() {
        for (k, wire) in [
            (ForgeKind::GitHub, "\"github\""),
            (ForgeKind::GitLab, "\"gitlab\""),
            (ForgeKind::Gitea, "\"gitea\""),
        ] {
            assert_eq!(serde_json::to_string(&k).unwrap(), wire);
            let back: ForgeKind = serde_json::from_str(wire).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn pull_request_spec_defaults_body_and_draft() {
        let spec: PullRequestSpec =
            serde_json::from_str(r#"{"head_branch":"feat","base_branch":"main","title":"t"}"#)
                .unwrap();
        assert_eq!(spec.body, "");
        assert!(!spec.draft);
    }
}
