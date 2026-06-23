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
use engram_core::traits::{CredentialHint, HarnessByteStream, Integration, ScopedCredential};
use engram_core::types::ids::SessionId;
use engram_core::types::Capability;
use engram_harness_proto::{read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{AssetSurface, FetchableRef, SessionEvent, SharedState};

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
/// configured forge. Layers the forge-configured check on top of the
/// shared [`crate::api::session_auth::authorize_broker_token`].
async fn authorize(
    state: &SharedState,
    session: SessionId,
    token: &str,
) -> Result<Arc<dyn Integration>, Denied> {
    let integration = state
        .integrations
        .resolve("github", &state.services.secrets)
        .await
        .ok_or(Denied::NoForge)?;
    if crate::api::session_auth::authorize_broker_token(state, session, token).await {
        Ok(integration)
    } else {
        Err(Denied::BadToken)
    }
}

/// Mint a credential, rejecting a host that doesn't match the forge.
///
/// ADR 0056 (Plane A): the token is scoped to the session's bound capabilities
/// — read them from PG and hand them to the mint, which clamps the GitHub App
/// permissions to exactly what the profile granted (filtering to its own
/// provider). An empty set falls back to the provider's default scopes.
async fn op_fetch_credential(
    state: &SharedState,
    session: SessionId,
    integration: &Arc<dyn Integration>,
    host: Option<String>,
    owner: Option<String>,
) -> Result<ScopedCredential, String> {
    let caps = state
        .services
        .meta
        .get_session_capabilities(session)
        .await
        .unwrap_or_default();
    // The integration validates the host against its own (e.g. github.com).
    let hint = CredentialHint {
        host: host.filter(|s| !s.is_empty()),
        owner: owner.filter(|s| !s.is_empty()),
    };
    integration
        .mint_credential(&caps, &hint)
        .await
        .map_err(|e| format!("mint git credential: {e}"))
}

/// Open a change request, then best-effort surface its URL on the
/// session event stream (the PR is the source of truth — an emit
/// failure is logged, not fatal).
async fn op_create_pull_request(
    state: &SharedState,
    session: SessionId,
    integration: &Arc<dyn Integration>,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    // ADR 0056 §4: PR creation is the mediated (server-performed) action. The
    // capability isn't yet cap-gated here (the broker token gates it; ADR 0057
    // D2 retired the `[git]` block); synthesize the `pulls:write` action the
    // GitHub integration matches.
    let cap = Capability {
        provider: "github".to_string(),
        action: "pulls:write".to_string(),
        resource: None,
    };
    let reply = integration
        .perform_action(&cap, &args)
        .await
        .map_err(|e| format!("create pull request: {e}"))?;
    // ADR 0056: a PR is one instance of the generic IntegrationAsset — a
    // durable `forge`/`pull_request` asset whose URL is an external link. The
    // wire stays semantic (provider + asset_kind + data); the web keys its
    // renderer on (provider, asset_kind). Built from the request args + the
    // real reply bytes.
    let number = reply
        .get("number")
        .or_else(|| reply.get("id"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let url = reply.get("url").and_then(|v| v.as_str()).map(String::from);
    if let Err(e) = state
        .emit(
            session,
            SessionEvent::IntegrationAsset {
                provider: "forge".into(),
                asset_kind: "pull_request".into(),
                surface: AssetSurface::Asset,
                data: serde_json::json!({
                    "repo": args.get("repo"),
                    "title": args.get("title"),
                    "number": number,
                    "head_branch": args.get("head_branch"),
                    "base_branch": args.get("base_branch"),
                }),
                fetchable: url.map(|url| FetchableRef::External { url }),
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session = %session, error = %e, "emit IntegrationAsset(forge/pull_request) failed (PR was created)");
    }
    Ok(reply)
}

use crate::api::session_auth::bearer;

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
    let integration = authorize(&state, id, &token).await?;
    let cred = op_fetch_credential(&state, id, &integration, q.host, q.owner)
        .await
        .map_err(ApiError::Internal)?;
    let ScopedCredential::Basic {
        username,
        password,
        expires_at,
    } = cred
    else {
        return Err(ApiError::Internal(
            "github mint returned a non-basic credential".into(),
        ));
    };
    Ok(Json(GitCredentialResponse {
        username,
        password,
        expires_at: expires_at.to_rfc3339(),
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
    let integration = authorize(&state, id, &token).await?;
    let args = serde_json::json!({
        "repo": req.repo,
        "head_branch": req.head_branch,
        "base_branch": req.base_branch,
        "title": req.title,
        "body": req.body,
        "draft": req.draft,
    });
    let reply = op_create_pull_request(&state, id, &integration, args)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(CreatePrResponse {
        url: reply["url"].as_str().unwrap_or_default().to_string(),
        id: reply["id"]
            .as_u64()
            .or_else(|| reply["number"].as_u64())
            .unwrap_or(0),
        state: reply["state"].as_str().unwrap_or_default().to_string(),
    }))
}

// ---- host-facing forwarder (Firecracker split mode) --------------------

/// `POST /api/hosts/forge` — ADR 0023 split-mode forge forwarding.
///
/// On a split deployment the FC host can't run the forge `ForgeSink`
/// locally: the sink needs the coord's `GitForge` + the broker-token
/// map, and the closure can't cross the coord↔host gRPC boundary
/// (`RemoteHostClient::set_forge_sink` is a no-op). So the host-agent
/// reads the in-guest [`ForgeRequest`] off the vsock stream and POSTs it
/// here; we run the exact same [`process_request`] core as the vsock
/// path and return the [`ForgeResponse`].
///
/// Mounted in the bearer-authed `/api/hosts/*` group (host identity).
/// The per-session broker token still rides in the body and is validated
/// by `process_request`, so this is not a way to bypass session scoping —
/// it's the same two-factor check the in-process path applies.
pub async fn forge_forward(
    State(state): State<SharedState>,
    Json(req): Json<ForgeRequest>,
) -> Json<ForgeResponse> {
    Json(process_request(&state, req).await)
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
    let integration = match authorize(state, req.session_id, &req.broker_token).await {
        Ok(f) => f,
        Err(d) => {
            return ForgeResponse::Error {
                message: d.message().into(),
            }
        }
    };
    match req.op {
        ForgeOp::FetchCredential { host, owner } => {
            match op_fetch_credential(state, req.session_id, &integration, Some(host), owner).await
            {
                Ok(ScopedCredential::Basic {
                    username, password, ..
                }) => ForgeResponse::Credential { username, password },
                Ok(_) => ForgeResponse::Error {
                    message: "github mint returned a non-basic credential".into(),
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
            let args = serde_json::json!({
                "repo": repo,
                "head_branch": head_branch,
                "base_branch": base_branch,
                "title": title,
                "body": body,
                "draft": draft,
            });
            match op_create_pull_request(state, req.session_id, &integration, args).await {
                Ok(reply) => ForgeResponse::PullRequest {
                    url: reply["url"].as_str().unwrap_or_default().to_string(),
                    id: reply["id"]
                        .as_u64()
                        .or_else(|| reply["number"].as_u64())
                        .unwrap_or(0),
                    state: reply["state"].as_str().unwrap_or_default().to_string(),
                },
                Err(message) => ForgeResponse::Error { message },
            }
        }
    }
}
