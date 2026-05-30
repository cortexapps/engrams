//! ADR 0023 in-session forge seam.
//!
//! Two operations the in-guest helpers invoke — mint a fresh git
//! credential, and open a change request (PR/MR) — reachable over two
//! transports that share the same auth + dispatch core:
//!
//! - **HTTP** (`GET /sessions/:id/git-credential`,
//!   `POST /sessions/:id/pull-request`): the ProcessBackend / `--mode=all`
//!   loopback path. Mounted OUTSIDE the deployment bearer layer; authed
//!   in-handler by the per-session broker token.
//! - **vsock** ([`handle_vsock_connection`]): the Firecracker path. The
//!   FC backend's forge listener fires this for each in-guest dial; we
//!   read a [`ForgeRequest`], run the same core, write a [`ForgeResponse`].
//!
//! Either way the guest holds only its session-scoped broker token
//! (`ENGRAM_FORGE_TOKEN`), never a deployment token.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use engram_core::traits::{
    GitForge, HarnessByteStream, PullRequest, PullRequestSpec, RepoRef, ScopedToken,
};
use engram_core::types::ids::SessionId;
use engram_harness_proto::{read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

// ---- shared auth + dispatch core (HTTP + vsock) ------------------------

/// Why a forge request was denied. Maps onto an HTTP status (HTTP path)
/// or a `ForgeResponse::Error` message (vsock path).
enum Denied {
    NoForge,
    BadToken,
}

impl Denied {
    fn message(&self) -> &'static str {
        match self {
            Self::NoForge => "no git forge is configured",
            Self::BadToken => "invalid or missing forge token",
        }
    }
}

impl From<Denied> for ApiError {
    fn from(d: Denied) -> Self {
        match d {
            Denied::NoForge => ApiError::Unsupported(d.message().into()),
            Denied::BadToken => ApiError::Unauthorized(d.message().into()),
        }
    }
}

/// Verify the per-session broker token (constant-time) and return the
/// configured forge. Shared by both transports.
fn authorize(
    state: &SharedState,
    session: SessionId,
    token: &str,
) -> Result<Arc<dyn GitForge>, Denied> {
    let forge = state.forge.clone().ok_or(Denied::NoForge)?;
    let ok = match state.git_broker_tokens.get(&session) {
        Some(expected) => constant_time_eq(token.as_bytes(), expected.value().as_bytes()),
        None => false,
    };
    if ok {
        Ok(forge)
    } else {
        Err(Denied::BadToken)
    }
}

/// Mint a credential, rejecting a host that doesn't match the forge.
async fn op_fetch_credential(
    forge: &Arc<dyn GitForge>,
    host: Option<String>,
    owner: Option<String>,
) -> Result<ScopedToken, String> {
    if let Some(h) = host.as_deref().filter(|s| !s.is_empty()) {
        if !h.eq_ignore_ascii_case(forge.host()) {
            return Err(format!(
                "no forge configured for host `{h}` (this coordinator serves `{}`)",
                forge.host()
            ));
        }
    }
    forge
        .mint_installation_token(owner.as_deref().filter(|s| !s.is_empty()))
        .await
        .map_err(|e| format!("mint git credential: {e}"))
}

/// Open a change request, then best-effort surface its URL on the
/// session event stream (the PR is the source of truth — an emit
/// failure is logged, not fatal).
async fn op_create_pull_request(
    state: &SharedState,
    session: SessionId,
    forge: &Arc<dyn GitForge>,
    repo: &str,
    spec: PullRequestSpec,
) -> Result<PullRequest, String> {
    let repo_ref = RepoRef::parse(repo).map_err(|e| format!("invalid repo: {e}"))?;
    let pr = forge
        .create_pull_request(&repo_ref, &spec)
        .await
        .map_err(|e| format!("create pull request: {e}"))?;
    if let Err(e) = state
        .emit(
            session,
            SessionEvent::PullRequestOpened {
                url: pr.url.clone(),
                repo: repo.to_string(),
                title: spec.title,
                number: pr.id,
                head_branch: spec.head_branch,
                base_branch: spec.base_branch,
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session = %session, error = %e, "emit PullRequestOpened failed (PR was created)");
    }
    Ok(pr)
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

// ---- HTTP transport ----------------------------------------------------

#[derive(Deserialize)]
pub struct GitCredentialQuery {
    #[serde(default)]
    pub host: Option<String>,
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

/// `GET /sessions/:id/git-credential` — mint a fresh installation
/// credential. The guest fetches per git op, so token expiry is
/// invisible in-guest.
pub async fn git_credential(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(q): Query<GitCredentialQuery>,
    headers: HeaderMap,
) -> Result<Json<GitCredentialResponse>, ApiError> {
    let token =
        bearer(&headers).ok_or_else(|| ApiError::Unauthorized("missing forge token".into()))?;
    let forge = authorize(&state, id, &token)?;
    let tok = op_fetch_credential(&forge, q.host, q.owner)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(GitCredentialResponse {
        username: tok.username,
        password: tok.password,
        expires_at: tok.expires_at.to_rfc3339(),
    }))
}

/// `POST /sessions/:id/pull-request` — open a change request against any
/// repo the installation can access.
pub async fn create_pull_request(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    headers: HeaderMap,
    Json(req): Json<CreatePrRequest>,
) -> Result<Json<CreatePrResponse>, ApiError> {
    let token =
        bearer(&headers).ok_or_else(|| ApiError::Unauthorized("missing forge token".into()))?;
    let forge = authorize(&state, id, &token)?;
    let spec = PullRequestSpec {
        head_branch: req.head_branch,
        base_branch: req.base_branch,
        title: req.title,
        body: req.body,
        draft: req.draft,
    };
    let pr = op_create_pull_request(&state, id, &forge, &req.repo, spec)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(CreatePrResponse {
        url: pr.url,
        id: pr.id,
        state: pr.state,
    }))
}

// ---- vsock transport (Firecracker) -------------------------------------

/// Handle one in-guest forge connection: read a [`ForgeRequest`], run
/// the shared core, write a [`ForgeResponse`]. Wired as the FC backend's
/// `ForgeSink` in `lib.rs`. Errors (including auth) come back as
/// `ForgeResponse::Error` rather than dropping the connection, so the
/// in-guest helper gets a usable message.
pub async fn handle_vsock_connection(state: SharedState, mut stream: HarnessByteStream) {
    let req: ForgeRequest = match read_msg(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "forge vsock: malformed request");
            return;
        }
    };
    let resp = process_request(&state, req).await;
    if let Err(e) = write_msg(&mut stream, &resp).await {
        tracing::debug!(error = %e, "forge vsock: response write failed");
    }
}

async fn process_request(state: &SharedState, req: ForgeRequest) -> ForgeResponse {
    let forge = match authorize(state, req.session_id, &req.broker_token) {
        Ok(f) => f,
        Err(d) => {
            return ForgeResponse::Error {
                message: d.message().into(),
            }
        }
    };
    match req.op {
        ForgeOp::FetchCredential { host, owner } => {
            match op_fetch_credential(&forge, Some(host), owner).await {
                Ok(t) => ForgeResponse::Credential {
                    username: t.username,
                    password: t.password,
                },
                Err(message) => ForgeResponse::Error { message },
            }
        }
        ForgeOp::CreatePullRequest {
            repo,
            head_branch,
            base_branch,
            title,
            body,
            draft,
        } => {
            let spec = PullRequestSpec {
                head_branch,
                base_branch,
                title,
                body,
                draft,
            };
            match op_create_pull_request(state, req.session_id, &forge, &repo, spec).await {
                Ok(pr) => ForgeResponse::PullRequest {
                    url: pr.url,
                    id: pr.id,
                    state: pr.state,
                },
                Err(message) => ForgeResponse::Error { message },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header::AUTHORIZATION, HeaderValue};

    #[test]
    fn bearer_extracts_only_bearer_scheme() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tok123"));
        assert_eq!(bearer(&h).as_deref(), Some("tok123"));
        assert!(bearer(&HeaderMap::new()).is_none());
        let mut basic = HeaderMap::new();
        basic.insert(AUTHORIZATION, HeaderValue::from_static("Basic tok123"));
        assert!(bearer(&basic).is_none());
    }

    #[test]
    fn constant_time_eq_compares_exactly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret", b"secres"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }
}
