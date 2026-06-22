//! GitHub App [`GitForge`] (ADR 0023).
//!
//! Authenticates as a GitHub App: sign a short-lived RS256 JWT with the
//! app private key, exchange it for a repo-scoped **installation access
//! token** (~1h), and use that token for git credentials + the pull
//! request API. Installation tokens are lazily cached and re-minted
//! shortly before expiry, so the coordinator hands the in-session
//! `GIT_ASKPASS` helper a fresh credential on every request without a
//! round-trip per git op.
//!
//! The app private key is the only long-lived secret and never leaves
//! the coordinator.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use engram_core::error::GitForgeError;
use engram_core::traits::{
    ForgeKind, GitForge, PullRequest, PullRequestSpec, RepoRef, ScopedToken,
};
use engram_core::types::Capability;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const GITHUB_API: &str = "https://api.github.com";
const GITHUB_HOST: &str = "github.com";
const API_VERSION: &str = "2022-11-28";

/// ADR 0056 (Plane A): compute the GitHub App `permissions` object from the
/// session's bound `github:` capabilities. A capability action is
/// `<resource>:<level>` (e.g. `contents:write`, `pulls:write`); the App
/// permission key is the resource with `pulls` → `pull_requests`, and the
/// level is the highest granted (read < write < admin). Returns an empty map
/// when no `github:` cap is bound — the caller then falls back to the default
/// scopes (today's behavior), so a capability-less profile is unaffected.
fn permissions_for_caps(caps: &[Capability]) -> BTreeMap<String, String> {
    let rank = |l: &str| match l {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for c in caps {
        if c.provider != "github" {
            continue;
        }
        let Some((resource, level)) = c.action.split_once(':') else {
            continue;
        };
        let key = match resource {
            "pulls" => "pull_requests",
            other => other,
        }
        .to_string();
        match out.get(&key) {
            Some(existing) if rank(existing) >= rank(level) => {}
            _ => {
                out.insert(key, level.to_string());
            }
        }
    }
    out
}

/// ADR 0056 (Plane A): restrict the minted token to specific repositories when
/// EVERY bound `github:` capability names one via its `resource`
/// (`owner/repo` or `repo` → the repo name). If any cap is unscoped (no
/// `resource`), the union is installation-wide → empty (no `repositories`
/// restriction), matching today's installation-wide token.
fn repositories_for_caps(caps: &[Capability]) -> Vec<String> {
    let github: Vec<&Capability> = caps.iter().filter(|c| c.provider == "github").collect();
    if github.is_empty() || github.iter().any(|c| c.resource.is_none()) {
        return Vec::new();
    }
    let mut repos: Vec<String> = github
        .iter()
        .filter_map(|c| c.resource.as_deref())
        .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
        .collect();
    repos.sort();
    repos.dedup();
    repos
}

#[derive(Serialize)]
struct JwtClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

/// A GitForge backed by a GitHub App installation.
pub struct GitHubApp {
    app_id: String,
    encoding_key: EncodingKey,
    http: reqwest::Client,
    base_url: String,
    /// repo slug → installation id (installations rarely change).
    installations: Mutex<HashMap<String, u64>>,
    /// repo slug → last minted token (lazily refreshed near expiry).
    tokens: Mutex<HashMap<String, ScopedToken>>,
}

impl GitHubApp {
    /// `app_id` is the GitHub App's numeric ID (as a string);
    /// `private_key_pem` is its RSA private key (PKCS#1 or PKCS#8 PEM).
    pub fn new(app_id: impl Into<String>, private_key_pem: &str) -> Result<Self, GitForgeError> {
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).map_err(|e| {
            GitForgeError::Unauthorized(format!("invalid GitHub App private key: {e}"))
        })?;
        let http = reqwest::Client::builder()
            .user_agent("engram-git-github")
            .build()
            .map_err(|e| GitForgeError::Backend(Box::new(e)))?;
        Ok(Self {
            app_id: app_id.into(),
            encoding_key,
            http,
            base_url: GITHUB_API.to_string(),
            installations: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    /// Override the API base URL (tests / GitHub Enterprise).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Mint a fresh app-level JWT (RS256, ≤10-min life). Coord-internal —
    /// never enters a sandbox.
    fn app_jwt(&self) -> Result<String, GitForgeError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| GitForgeError::Backend(Box::new(e)))?
            .as_secs();
        let claims = JwtClaims {
            iat: now - 60,  // backdate for clock skew
            exp: now + 540, // 9 min (GitHub caps at 10)
            iss: self.app_id.clone(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .map_err(|e| GitForgeError::Unauthorized(format!("sign app JWT: {e}")))
    }

    /// Resolve the installation id for `owner` (org first, then user),
    /// or the app's sole installation when `owner` is `None`. Cached by
    /// owner key (`""` for the default).
    async fn installation_id(&self, owner: Option<&str>) -> Result<u64, GitForgeError> {
        let key = owner.unwrap_or("").to_string();
        if let Some(id) = self.installations.lock().get(&key).copied() {
            return Ok(id);
        }
        let jwt = self.app_jwt()?;
        let id = match owner {
            // Org installs are the common case; fall back to a user
            // install on 404.
            Some(o) => match self
                .get_installation_id(&jwt, &format!("{}/orgs/{}/installation", self.base_url, o))
                .await
            {
                Ok(id) => id,
                Err(GitForgeError::NotFound(_)) => {
                    self.get_installation_id(
                        &jwt,
                        &format!("{}/users/{}/installation", self.base_url, o),
                    )
                    .await?
                }
                Err(e) => return Err(e),
            },
            None => {
                let url = format!("{}/app/installations", self.base_url);
                let resp = self
                    .http
                    .get(&url)
                    .bearer_auth(&jwt)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", API_VERSION)
                    .send()
                    .await
                    .map_err(|e| GitForgeError::Backend(Box::new(e)))?;
                let resp = ensure_ok(resp, "list installations").await?;
                #[derive(Deserialize)]
                struct Inst {
                    id: u64,
                }
                let insts: Vec<Inst> = resp
                    .json()
                    .await
                    .map_err(|e| GitForgeError::Protocol(format!("installations json: {e}")))?;
                match insts.as_slice() {
                    [one] => one.id,
                    [] => return Err(GitForgeError::NotFound("app has no installations".into())),
                    _ => {
                        return Err(GitForgeError::InvalidSpec(
                            "app spans multiple installations; specify an owner".into(),
                        ))
                    }
                }
            }
        };
        self.installations.lock().insert(key, id);
        Ok(id)
    }

    async fn get_installation_id(&self, jwt: &str, url: &str) -> Result<u64, GitForgeError> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|e| GitForgeError::Backend(Box::new(e)))?;
        let resp = ensure_ok(resp, "look up installation").await?;
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        let inst: Installation = resp
            .json()
            .await
            .map_err(|e| GitForgeError::Protocol(format!("installation json: {e}")))?;
        Ok(inst.id)
    }
}

#[async_trait]
impl GitForge for GitHubApp {
    async fn mint_installation_token(
        &self,
        caps: &[Capability],
        owner: Option<&str>,
    ) -> Result<ScopedToken, GitForgeError> {
        // ADR 0056 (Plane A): scope the token to the session's bound caps.
        let permissions = permissions_for_caps(caps);
        let repositories = repositories_for_caps(caps);
        // The cache key MUST include the scope: two sessions with the same
        // owner but different capabilities get DIFFERENT tokens — otherwise a
        // read-only session could be served a cached write token.
        let perm_fp = permissions
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let key = format!(
            "{}|{}|{}",
            owner.unwrap_or(""),
            perm_fp,
            repositories.join(",")
        );
        // Serve from cache while comfortably inside the validity window.
        if let Some(tok) = self.tokens.lock().get(&key) {
            if tok.expires_at > Utc::now() + Duration::minutes(5) {
                return Ok(tok.clone());
            }
        }
        let id = self.installation_id(owner).await?;
        let jwt = self.app_jwt()?;
        let url = format!("{}/app/installations/{}/access_tokens", self.base_url, id);
        // Permissions are computed from the bound caps; an empty set (a
        // capability-less profile) falls back to the historical default scopes
        // so existing forge-bound sessions are unaffected. `repositories` is
        // added only when every cap names one (else installation-wide).
        let permissions_json = if permissions.is_empty() {
            serde_json::json!({ "contents": "write", "pull_requests": "write" })
        } else {
            serde_json::to_value(&permissions)
                .map_err(|e| GitForgeError::Protocol(format!("permissions json: {e}")))?
        };
        let mut body = serde_json::json!({ "permissions": permissions_json });
        if !repositories.is_empty() {
            body["repositories"] = serde_json::json!(repositories);
        }
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| GitForgeError::Backend(Box::new(e)))?;
        let resp = ensure_ok(resp, "mint installation token").await?;
        #[derive(Deserialize)]
        struct TokenResp {
            token: String,
            expires_at: DateTime<Utc>,
        }
        let tr: TokenResp = resp
            .json()
            .await
            .map_err(|e| GitForgeError::Protocol(format!("token json: {e}")))?;
        let scoped = ScopedToken {
            username: "x-access-token".to_string(),
            password: tr.token,
            expires_at: tr.expires_at,
        };
        self.tokens.lock().insert(key, scoped.clone());
        Ok(scoped)
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        pr: &PullRequestSpec,
    ) -> Result<PullRequest, GitForgeError> {
        // The mediated PR action uses the provider's default scopes (an empty
        // cap set) — PR creation needs pull_requests + contents write, the same
        // scopes the forge minted before ADR 0056's capability scoping.
        let token = self.mint_installation_token(&[], Some(&repo.owner)).await?;
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, repo.owner, repo.name);
        let body = serde_json::json!({
            "title": pr.title,
            "head": pr.head_branch,
            "base": pr.base_branch,
            "body": pr.body,
            "draft": pr.draft,
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token.password)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| GitForgeError::Backend(Box::new(e)))?;
        let resp = ensure_ok(resp, "create pull request").await?;
        #[derive(Deserialize)]
        struct PrResp {
            html_url: String,
            number: u64,
            state: String,
        }
        let pr_resp: PrResp = resp
            .json()
            .await
            .map_err(|e| GitForgeError::Protocol(format!("pull request json: {e}")))?;
        Ok(PullRequest {
            url: pr_resp.html_url,
            id: pr_resp.number,
            state: pr_resp.state,
        })
    }

    fn host(&self) -> &str {
        GITHUB_HOST
    }

    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }
}

/// Map a non-2xx GitHub response onto a typed `GitForgeError`, folding a
/// truncated body in for diagnostics.
async fn ensure_ok(resp: reqwest::Response, ctx: &str) -> Result<reqwest::Response, GitForgeError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(300).collect();
    let msg = format!("{ctx}: HTTP {status}: {snippet}");
    Err(match status.as_u16() {
        401 | 403 => GitForgeError::Unauthorized(msg),
        404 => GitForgeError::NotFound(msg),
        422 => GitForgeError::Rejected(msg),
        _ => GitForgeError::Protocol(msg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(specs: &[&str]) -> Vec<Capability> {
        specs
            .iter()
            .map(|s| Capability::parse(s).unwrap())
            .collect()
    }

    #[test]
    fn permissions_map_action_to_github_key_and_level() {
        let p = permissions_for_caps(&caps(&["github:contents:read", "github:pulls:write"]));
        assert_eq!(p.get("contents").map(String::as_str), Some("read"));
        // `pulls` action → GitHub's `pull_requests` permission key.
        assert_eq!(p.get("pull_requests").map(String::as_str), Some("write"));
    }

    #[test]
    fn permissions_keep_the_highest_level() {
        let p = permissions_for_caps(&caps(&["github:contents:read", "github:contents:write"]));
        assert_eq!(p.get("contents").map(String::as_str), Some("write"));
    }

    #[test]
    fn permissions_ignore_other_providers() {
        let p = permissions_for_caps(&caps(&["datadog:logs:read", "github:issues:write"]));
        assert_eq!(p.len(), 1);
        assert_eq!(p.get("issues").map(String::as_str), Some("write"));
    }

    #[test]
    fn empty_caps_yield_empty_permissions() {
        // The caller falls back to the default scopes for an empty set.
        assert!(permissions_for_caps(&[]).is_empty());
    }

    #[test]
    fn repositories_restrict_only_when_every_cap_is_scoped() {
        // All scoped → restrict to those repo names.
        let repos = repositories_for_caps(&caps(&[
            "github:contents:write@cortexapps/engrams",
            "github:pulls:write@cortexapps/engrams",
        ]));
        assert_eq!(repos, vec!["engrams".to_string()]);

        // Any unscoped cap → installation-wide (no restriction).
        let mixed = repositories_for_caps(&caps(&[
            "github:contents:write@cortexapps/engrams",
            "github:pulls:write",
        ]));
        assert!(mixed.is_empty());
    }
}
