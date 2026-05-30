//! ADR 0023 in-session forge seam.
//!
//! Two endpoints the in-guest `GIT_ASKPASS` helper / `engram-pr` script
//! call, authenticated by the per-session credential-broker token
//! (minted at session create, injected as `ENGRAM_FORGE_TOKEN`):
//!
//! - `GET  /sessions/:id/git-credential` — mint a fresh installation
//!   credential (valid across the installation's repos).
//! - `POST /sessions/:id/pull-request`   — open a change request (PR/MR).
//!
//! Mounted OUTSIDE the deployment bearer-auth layer: the guest holds
//! only its session-scoped broker token, never a deployment token.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use engram_core::traits::{GitForge, PullRequestSpec, RepoRef};
use engram_core::types::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

#[derive(Deserialize)]
pub struct GitCredentialQuery {
    /// Git host the credential is for (e.g. `github.com`). The helper
    /// passes it so a future multi-forge coord can route; unused today.
    #[serde(default)]
    pub host: Option<String>,
    /// Installation owner (org/user) to scope to. Empty/absent → the
    /// forge's sole installation.
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Serialize)]
pub struct GitCredentialResponse {
    pub username: String,
    pub password: String,
    pub expires_at: String,
}

#[derive(Deserialize)]
pub struct CreatePrRequest {
    /// Target repo `owner/name` — any repo the installation can access.
    pub repo: String,
    pub head_branch: String,
    pub base_branch: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub draft: bool,
}

#[derive(Serialize)]
pub struct CreatePrResponse {
    pub url: String,
    pub id: u64,
    pub state: String,
}

/// Authenticate a forge request and return the configured forge. The
/// caller must present the session's broker token as
/// `Authorization: Bearer <token>` (constant-time compared).
fn authn(
    state: &SharedState,
    session: SessionId,
    headers: &HeaderMap,
) -> Result<Arc<dyn GitForge>, ApiError> {
    let forge = state
        .forge
        .clone()
        .ok_or_else(|| ApiError::Unsupported("no git forge is configured".into()))?;
    let presented =
        bearer(headers).ok_or_else(|| ApiError::Unauthorized("missing forge token".into()))?;
    let ok = match state.git_broker_tokens.get(&session) {
        Some(expected) => constant_time_eq(presented.as_bytes(), expected.value().as_bytes()),
        None => false,
    };
    if !ok {
        return Err(ApiError::Unauthorized("invalid forge token".into()));
    }
    Ok(forge)
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    v.strip_prefix("Bearer ").map(str::to_string)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `GET /sessions/:id/git-credential` — mint a fresh installation
/// credential. The guest fetches per git op, so token expiry is
/// invisible in-guest.
pub async fn git_credential(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(q): Query<GitCredentialQuery>,
    headers: HeaderMap,
) -> Result<Json<GitCredentialResponse>, ApiError> {
    let forge = authn(&state, id, &headers)?;
    // If the helper named a host, it must be the one this forge serves —
    // don't mint a github token for a gitlab.com request.
    if let Some(h) = q.host.as_deref().filter(|s| !s.is_empty()) {
        if !h.eq_ignore_ascii_case(forge.host()) {
            return Err(ApiError::BadRequest(format!(
                "no forge configured for host `{h}` (this coordinator serves `{}`)",
                forge.host()
            )));
        }
    }
    let token = forge
        .mint_installation_token(q.owner.as_deref().filter(|s| !s.is_empty()))
        .await
        .map_err(|e| ApiError::Internal(format!("mint git credential: {e}")))?;
    Ok(Json(GitCredentialResponse {
        username: token.username,
        password: token.password,
        expires_at: token.expires_at.to_rfc3339(),
    }))
}

/// `POST /sessions/:id/pull-request` — open a change request against any
/// repo the installation can access, then surface its URL on the
/// session event stream.
pub async fn create_pull_request(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    headers: HeaderMap,
    Json(req): Json<CreatePrRequest>,
) -> Result<Json<CreatePrResponse>, ApiError> {
    let forge = authn(&state, id, &headers)?;
    let repo = RepoRef::parse(&req.repo)
        .map_err(|e| ApiError::BadRequest(format!("invalid repo: {e}")))?;
    let spec = PullRequestSpec {
        head_branch: req.head_branch,
        base_branch: req.base_branch,
        title: req.title,
        body: req.body,
        draft: req.draft,
    };
    let pr = forge
        .create_pull_request(&repo, &spec)
        .await
        .map_err(|e| ApiError::Internal(format!("create pull request: {e}")))?;
    state
        .emit(
            id,
            SessionEvent::PullRequestOpened {
                url: pr.url.clone(),
                repo: req.repo,
                head_branch: spec.head_branch,
                base_branch: spec.base_branch,
                at: chrono::Utc::now(),
            },
        )
        .await?;
    Ok(Json(CreatePrResponse {
        url: pr.url,
        id: pr.id,
        state: pr.state,
    }))
}
