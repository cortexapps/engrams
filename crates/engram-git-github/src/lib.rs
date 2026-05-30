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

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use engram_core::error::GitForgeError;
use engram_core::traits::{
    ForgeKind, GitForge, PullRequest, PullRequestSpec, RepoRef, ScopedToken,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const GITHUB_API: &str = "https://api.github.com";
const GITHUB_HOST: &str = "github.com";
const API_VERSION: &str = "2022-11-28";

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

    async fn installation_id(&self, repo: &RepoRef) -> Result<u64, GitForgeError> {
        let slug = repo.to_string();
        if let Some(id) = self.installations.lock().get(&slug).copied() {
            return Ok(id);
        }
        let jwt = self.app_jwt()?;
        let url = format!(
            "{}/repos/{}/{}/installation",
            self.base_url, repo.owner, repo.name
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&jwt)
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
        self.installations.lock().insert(slug, inst.id);
        Ok(inst.id)
    }
}

#[async_trait]
impl GitForge for GitHubApp {
    async fn mint_repo_token(&self, repo: &RepoRef) -> Result<ScopedToken, GitForgeError> {
        let slug = repo.to_string();
        // Serve from cache while comfortably inside the validity window.
        if let Some(tok) = self.tokens.lock().get(&slug) {
            if tok.expires_at > Utc::now() + Duration::minutes(5) {
                return Ok(tok.clone());
            }
        }
        let id = self.installation_id(repo).await?;
        let jwt = self.app_jwt()?;
        let url = format!("{}/app/installations/{}/access_tokens", self.base_url, id);
        let body = serde_json::json!({
            "repositories": [repo.name],
            "permissions": { "contents": "write", "pull_requests": "write" },
        });
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
        self.tokens.lock().insert(slug, scoped.clone());
        Ok(scoped)
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        pr: &PullRequestSpec,
    ) -> Result<PullRequest, GitForgeError> {
        let token = self.mint_repo_token(repo).await?;
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
