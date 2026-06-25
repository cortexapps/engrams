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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use engram_core::error::IntegrationError;
use engram_core::traits::{
    CredentialHint, Integration, MintFieldKind, MintFieldSchema, MintKindDescriptor,
    ResolvedFields, ScopedCredential,
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

fn perm_rank(level: &str) -> u8 {
    match level {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// Clamp `requested` App permissions to what the installation actually `granted`:
/// keep a permission only if granted, capping its level at the granted level.
/// Returns `(clamped, dropped)` where `dropped` describes each requested
/// permission the installation can't fully satisfy.
///
/// GitHub's create-installation-token API is **all-or-nothing**: if any requested
/// permission exceeds the App's grant it 422s the *entire* token, so one
/// over-broad capability (e.g. `secrets:read` on an App without `secrets`) breaks
/// ALL github access — git push and the API alike. Clamping degrades that to "the
/// over-broad capability simply doesn't work" while everything the App can grant
/// keeps working.
fn clamp_permissions(
    requested: &BTreeMap<String, String>,
    granted: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut clamped = BTreeMap::new();
    let mut dropped = Vec::new();
    for (perm, want) in requested {
        match granted.get(perm) {
            Some(have) if perm_rank(have) >= perm_rank(want) => {
                clamped.insert(perm.clone(), want.clone());
            }
            Some(have) => {
                // Granted, but at a lower level — take the lower level.
                clamped.insert(perm.clone(), have.clone());
                dropped.push(format!(
                    "{perm} (wanted {want}, installation grants {have})"
                ));
            }
            None => dropped.push(format!(
                "{perm} (wanted {want}, not granted to the installation)"
            )),
        }
    }
    (clamped, dropped)
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

/// An [`Integration`] backed by a GitHub App installation (ADR 0056).
pub struct GitHubApp {
    app_id: String,
    encoding_key: EncodingKey,
    http: reqwest::Client,
    base_url: String,
    /// owner → installation id (installations rarely change).
    installations: Mutex<HashMap<String, u64>>,
    /// owner → the permissions the installation actually grants (the App's
    /// configured access). Used to clamp a token request so one over-broad
    /// capability can't 422 the whole all-or-nothing mint. Rarely changes.
    installation_perms: Mutex<HashMap<String, BTreeMap<String, String>>>,
    /// scope key → last minted credential (lazily refreshed near expiry).
    tokens: Mutex<HashMap<String, ScopedCredential>>,
}

impl GitHubApp {
    /// `app_id` is the GitHub App's numeric ID (as a string);
    /// `private_key_pem` is its RSA private key (PKCS#1 or PKCS#8 PEM).
    pub fn new(app_id: impl Into<String>, private_key_pem: &str) -> Result<Self, IntegrationError> {
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).map_err(|e| {
            IntegrationError::Unauthorized(format!("invalid GitHub App private key: {e}"))
        })?;
        let http = reqwest::Client::builder()
            .user_agent("engram-git-github")
            .build()
            .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
        Ok(Self {
            app_id: app_id.into(),
            encoding_key,
            http,
            base_url: GITHUB_API.to_string(),
            installations: Mutex::new(HashMap::new()),
            installation_perms: Mutex::new(HashMap::new()),
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
    fn app_jwt(&self) -> Result<String, IntegrationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| IntegrationError::Backend(Box::new(e)))?
            .as_secs();
        let claims = JwtClaims {
            iat: now - 60,  // backdate for clock skew
            exp: now + 540, // 9 min (GitHub caps at 10)
            iss: self.app_id.clone(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .map_err(|e| IntegrationError::Unauthorized(format!("sign app JWT: {e}")))
    }

    /// Resolve the installation id for `owner` (org first, then user),
    /// or the app's sole installation when `owner` is `None`. Cached by
    /// owner key (`""` for the default).
    async fn installation_id(&self, owner: Option<&str>) -> Result<u64, IntegrationError> {
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
                Err(IntegrationError::NotFound(_)) => {
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
                    .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
                let resp = ensure_ok(resp, "list installations").await?;
                #[derive(Deserialize)]
                struct Inst {
                    id: u64,
                }
                let insts: Vec<Inst> = resp
                    .json()
                    .await
                    .map_err(|e| IntegrationError::Protocol(format!("installations json: {e}")))?;
                match insts.as_slice() {
                    [one] => one.id,
                    [] => {
                        return Err(IntegrationError::NotFound(
                            "app has no installations".into(),
                        ))
                    }
                    _ => {
                        return Err(IntegrationError::InvalidSpec(
                            "app spans multiple installations; specify an owner".into(),
                        ))
                    }
                }
            }
        };
        self.installations.lock().insert(key, id);
        Ok(id)
    }

    async fn get_installation_id(&self, jwt: &str, url: &str) -> Result<u64, IntegrationError> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
        let resp = ensure_ok(resp, "look up installation").await?;
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        let inst: Installation = resp
            .json()
            .await
            .map_err(|e| IntegrationError::Protocol(format!("installation json: {e}")))?;
        Ok(inst.id)
    }

    /// The permissions the installation actually grants (cached by owner key),
    /// read off the installation object. Used only to clamp a token request after
    /// a 422 (see [`Self::mint_basic`]); best-effort, so callers treat an error as
    /// "unknown grant" and skip clamping.
    async fn installation_permissions(
        &self,
        owner: Option<&str>,
        id: u64,
    ) -> Result<BTreeMap<String, String>, IntegrationError> {
        let key = owner.unwrap_or("").to_string();
        if let Some(p) = self.installation_perms.lock().get(&key).cloned() {
            return Ok(p);
        }
        let jwt = self.app_jwt()?;
        let url = format!("{}/app/installations/{}", self.base_url, id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
        let resp = ensure_ok(resp, "look up installation permissions").await?;
        #[derive(Deserialize)]
        struct InstPerms {
            #[serde(default)]
            permissions: BTreeMap<String, String>,
        }
        let ip: InstPerms = resp.json().await.map_err(|e| {
            IntegrationError::Protocol(format!("installation permissions json: {e}"))
        })?;
        self.installation_perms
            .lock()
            .insert(key, ip.permissions.clone());
        Ok(ip.permissions)
    }
}

impl GitHubApp {
    /// Mint an installation token scoped to `caps` — the
    /// `ScopedCredential::Basic` the git askpass helper wields. Shared by
    /// [`Integration::mint_credential`] and the mediated PR action.
    async fn mint_basic(
        &self,
        caps: &[Capability],
        owner: Option<&str>,
    ) -> Result<ScopedCredential, IntegrationError> {
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
        {
            let cache = self.tokens.lock();
            if let Some(c @ ScopedCredential::Basic { expires_at, .. }) = cache.get(&key) {
                if *expires_at > Utc::now() + Duration::minutes(5) {
                    return Ok(c.clone());
                }
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
                .map_err(|e| IntegrationError::Protocol(format!("permissions json: {e}")))?
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
            .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
        // GitHub's create-installation-token API is all-or-nothing: a 422 here is
        // almost always "the permissions requested are not granted to this
        // installation" — one over-broad capability (e.g. `secrets:read` on an App
        // without `secrets`) poisons the WHOLE request, breaking git push + the API
        // alike. Clamp to the installation's actual grant and retry once, so the
        // capabilities the App can satisfy still work and only the over-broad ones
        // drop out. (`permissions.is_empty()` uses the default scopes below — nothing
        // to clamp.)
        let resp = if resp.status().as_u16() == 422 && !permissions.is_empty() {
            let granted = self
                .installation_permissions(owner, id)
                .await
                .unwrap_or_default();
            let (clamped, dropped) = clamp_permissions(&permissions, &granted);
            if granted.is_empty() || clamped.is_empty() || clamped == permissions {
                // Couldn't learn the grant, the App grants nothing requested, or
                // there was nothing to clamp — surface the original 422 (don't loop).
                ensure_ok(resp, "mint installation token").await?
            } else {
                tracing::warn!(
                    owner = owner.unwrap_or("<default>"),
                    dropped = ?dropped,
                    "github mint: installation-token 422; retried with permissions clamped to the \
                     App's grant. Grant these to the App (or trim the profile's caps) to use them.",
                );
                let mut body2 = serde_json::json!({
                    "permissions": serde_json::to_value(&clamped)
                        .map_err(|e| IntegrationError::Protocol(format!("permissions json: {e}")))?,
                });
                if !repositories.is_empty() {
                    body2["repositories"] = serde_json::json!(repositories);
                }
                let resp2 = self
                    .http
                    .post(&url)
                    .bearer_auth(&jwt)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", API_VERSION)
                    .json(&body2)
                    .send()
                    .await
                    .map_err(|e| IntegrationError::Backend(Box::new(e)))?;
                ensure_ok(resp2, "mint installation token (clamped to App grant)").await?
            }
        } else {
            ensure_ok(resp, "mint installation token").await?
        };
        #[derive(Deserialize)]
        struct TokenResp {
            token: String,
            expires_at: DateTime<Utc>,
        }
        let tr: TokenResp = resp
            .json()
            .await
            .map_err(|e| IntegrationError::Protocol(format!("token json: {e}")))?;
        let scoped = ScopedCredential::Basic {
            username: "x-access-token".to_string(),
            password: tr.token,
            expires_at: tr.expires_at,
        };
        self.tokens.lock().insert(key, scoped.clone());
        Ok(scoped)
    }
}

#[async_trait]
impl Integration for GitHubApp {
    fn provider(&self) -> &str {
        "github"
    }

    async fn mint_credential(
        &self,
        caps: &[Capability],
        hint: &CredentialHint,
    ) -> Result<ScopedCredential, IntegrationError> {
        // served_host is our git host (github.com), NOT the API endpoint
        // (api.github.com); a caller that hands us the API host is mismatched.
        if let Some(h) = hint.served_host.as_deref().filter(|s| !s.is_empty()) {
            if !h.eq_ignore_ascii_case(GITHUB_HOST) {
                return Err(IntegrationError::InvalidSpec(format!(
                    "github integration does not serve host `{h}`"
                )));
            }
        }
        self.mint_basic(caps, hint.owner.as_deref().filter(|s| !s.is_empty()))
            .await
    }

    /// The github installation token is canonically a **bearer** for the REST API
    /// (the `Basic{username:"x-access-token", …}` shape exists for the git askpass
    /// helper). Inject it as `Authorization: Bearer <token>` so the same minted
    /// credential authenticates API calls on the egress inject plane (ADR 0056
    /// amendment) — and as a worked example of a per-provider header override.
    fn inject_header(
        &self,
        cred: &engram_core::traits::ScopedCredential,
    ) -> Option<engram_core::traits::InjectHeader> {
        if let engram_core::traits::ScopedCredential::Basic { password, .. } = cred {
            return Some(engram_core::traits::InjectHeader {
                name: "Authorization".to_string(),
                value: format!("Bearer {password}"),
            });
        }
        engram_core::traits::default_inject_header(cred)
    }

    /// github is a git forge served at `github.com`; its clone/push needs the
    /// askpass credential-helper seam (ADR 0056 P2). De-hardcodes the forge wiring.
    fn git_forge_host(&self) -> Option<String> {
        Some(GITHUB_HOST.to_string())
    }
}

/// Map a non-2xx GitHub response onto a typed `IntegrationError`, folding a
/// truncated body in for diagnostics.
async fn ensure_ok(
    resp: reqwest::Response,
    ctx: &str,
) -> Result<reqwest::Response, IntegrationError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(300).collect();
    let msg = format!("{ctx}: HTTP {status}: {snippet}");
    Err(match status.as_u16() {
        401 | 403 => IntegrationError::Unauthorized(msg),
        404 => IntegrationError::NotFound(msg),
        422 => IntegrationError::Rejected(msg),
        _ => IntegrationError::Protocol(msg),
    })
}

/// The `github_app` mint kind (ADR 0057 C2): a GitHub App authenticated by App
/// ID + RSA private-key PEM. An admin supplies both via the integrations UI (the
/// PEM sealed in the org-secret store); the coordinator resolves the two fields
/// (org secrets `github_app.app_id` / `github_app.private_key_pem`, name =
/// `<kind>.<field>`) and calls `build` to construct the engine lazily. This is
/// the data-driven frame around minting — the mint logic stays in [`GitHubApp`].
pub fn github_app_descriptor() -> MintKindDescriptor {
    MintKindDescriptor {
        kind: "github_app",
        provider: "github",
        display_name: "GitHub App",
        fields: vec![
            MintFieldSchema {
                name: "app_id",
                label: "App ID",
                field_kind: MintFieldKind::Config,
                required: true,
            },
            MintFieldSchema {
                name: "private_key_pem",
                label: "Private key (PEM)",
                field_kind: MintFieldKind::SealedSecret,
                required: true,
            },
        ],
        build: |fields: &ResolvedFields| {
            let app_id = fields.get("app_id").ok_or_else(|| {
                IntegrationError::InvalidSpec("github_app mint: missing app_id".into())
            })?;
            let pem = fields.get("private_key_pem").ok_or_else(|| {
                IntegrationError::InvalidSpec("github_app mint: missing private_key_pem".into())
            })?;
            Ok(Arc::new(GitHubApp::new(app_id.clone(), pem)?) as Arc<dyn Integration>)
        },
    }
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

    fn perms(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn clamp_drops_permissions_the_app_lacks() {
        // The prod incident (session 2f25d473): the profile requested secrets/actions
        // the App installation doesn't grant → GitHub 422'd the WHOLE token, breaking
        // git push + the API. Clamp keeps what the App grants (so push + PRs work) and
        // drops the rest.
        let requested = permissions_for_caps(&caps(&[
            "github:contents:write",
            "github:pulls:write",
            "github:issues:write",
            "github:secrets:read",
            "github:actions:read",
        ]));
        let granted = perms(&[
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "write"),
            ("metadata", "read"),
        ]);
        let (clamped, dropped) = clamp_permissions(&requested, &granted);
        assert_eq!(clamped.get("contents").map(String::as_str), Some("write"));
        assert_eq!(
            clamped.get("pull_requests").map(String::as_str),
            Some("write")
        );
        assert_eq!(clamped.get("issues").map(String::as_str), Some("write"));
        assert!(!clamped.contains_key("secrets"));
        assert!(!clamped.contains_key("actions"));
        assert_eq!(dropped.len(), 2, "secrets + actions reported as dropped");
    }

    #[test]
    fn clamp_caps_level_to_the_grant() {
        // Requested write, App grants only read → take read, report the downgrade.
        let (clamped, dropped) = clamp_permissions(
            &perms(&[("contents", "write")]),
            &perms(&[("contents", "read")]),
        );
        assert_eq!(clamped.get("contents").map(String::as_str), Some("read"));
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn clamp_is_a_noop_when_grant_covers_the_request() {
        let requested = perms(&[("contents", "write"), ("pull_requests", "write")]);
        let granted = perms(&[
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "write"),
        ]);
        let (clamped, dropped) = clamp_permissions(&requested, &granted);
        assert_eq!(clamped, requested);
        assert!(dropped.is_empty());
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
