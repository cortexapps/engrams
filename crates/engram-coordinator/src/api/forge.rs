//! ADRs 0023 and 0106 in-session credential-control seam.
//!
//! The in-guest `GIT_ASKPASS` helper asks for a fresh git clone/push
//! credential, and the harness asks for its OAuth credential, over two
//! transports that share one auth + dispatch core:
//!
//! - **HTTP** (`GET /sessions/:id/git-credential`): the ProcessBackend /
//!   `--mode=all` loopback path. Mounted OUTSIDE the deployment bearer
//!   layer; authed in-handler by the per-session broker token.
//! - **vsock** ([`handle_vsock_connection`]): the Firecracker path. The
//!   FC backend's forge listener fires this for each in-guest dial; we
//!   read a [`ForgeRequest`], run the same core, write a [`ForgeResponse`].
//!
//! Either way the guest holds only a session-scoped broker token, never a
//! deployment token. OAuth operations select no subject/provider from guest
//! input: the coordinator resolves the durable session binding.
//!
//! Only a forge whose integration mints a short-lived credential is served
//! here. A forge with a static token gets it from the egress proxy as a
//! header, so the token never enters the guest.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::traits::{CredentialHint, HarnessByteStream, Integration, ScopedCredential};
use engram_core::types::ids::SessionId;
use engram_core::types::Capability;
use engram_harness_proto::{read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::SharedState;

// ---- shared core (HTTP + vsock) -----------------------------------------

/// A git forge the session may use: a provider among the session's
/// capabilities whose integration serves a git host.
pub(crate) struct SessionForge {
    /// The capability provider this forge was resolved for.
    pub provider: String,
    pub integration: Arc<dyn Integration>,
    pub host: String,
}

/// The session's forges, one per capability provider that serves a git host.
pub(crate) async fn session_forges(state: &SharedState, caps: &[Capability]) -> Vec<SessionForge> {
    let providers: BTreeSet<&str> = caps.iter().map(|c| c.provider.as_str()).collect();
    let mut forges = Vec::new();
    for provider in providers {
        let Some(integration) = state
            .integrations
            .resolve(provider, &state.services.secrets)
            .await
        else {
            continue;
        };
        if let Some(host) = integration.git_forge_host() {
            forges.push(SessionForge {
                provider: provider.to_string(),
                integration,
                host,
            });
        }
    }
    forges
}

/// Why the session got no git credential.
enum GitCredentialError {
    /// The session holds no capability for a forge that serves the host.
    NoForge(&'static str),
    Failed(String),
}

impl GitCredentialError {
    fn into_message(self) -> String {
        match self {
            Self::NoForge(message) => message.to_string(),
            Self::Failed(message) => message,
        }
    }
}

impl From<GitCredentialError> for ApiError {
    fn from(error: GitCredentialError) -> Self {
        match error {
            GitCredentialError::NoForge(message) => ApiError::Forbidden(message.into()),
            GitCredentialError::Failed(message) => ApiError::Internal(message),
        }
    }
}

/// A git credential: the username and password git sends as HTTP Basic.
struct GitCredential {
    username: String,
    password: String,
    expires_at: DateTime<Utc>,
}

/// Mint the session's git credential for `host`. The forge is the one among
/// the session's capability providers that serves `host`; with no `host`, the
/// session's only forge. The mint clamps the credential to the capabilities,
/// so a session with no capability for a forge gets no credential from it.
async fn fetch_git_credential(
    state: &SharedState,
    session: SessionId,
    host: Option<String>,
    owner: Option<String>,
) -> Result<GitCredential, GitCredentialError> {
    use GitCredentialError::{Failed, NoForge};
    let host = host.filter(|s| !s.is_empty());
    let caps = state
        .services
        .meta
        .get_session_capabilities(session)
        .await
        .map_err(|e| Failed(format!("read session capabilities: {e}")))?;
    let mut forges = session_forges(state, &caps).await;
    if let Some(host) = &host {
        forges.retain(|forge| forge.host.eq_ignore_ascii_case(host));
    }
    let forge = match forges.as_slice() {
        [forge] => forge,
        [] => return Err(NoForge("this session has no git forge for that host")),
        _ => {
            return Err(NoForge(
                "this session has more than one git forge; name the host",
            ))
        }
    };
    let hint = CredentialHint {
        served_host: host,
        owner: owner.filter(|s| !s.is_empty()),
    };
    match forge.integration.mint_credential(&caps, &hint).await {
        Ok(ScopedCredential::Basic {
            username,
            password,
            expires_at,
        }) => Ok(GitCredential {
            username,
            password,
            expires_at,
        }),
        Ok(_) => Err(Failed(format!(
            "{} minted a credential git cannot send as HTTP Basic",
            forge.provider
        ))),
        Err(e) => Err(Failed(format!("mint git credential: {e}"))),
    }
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

/// `GET /sessions/:id/git-credential` — mint a fresh git credential. The
/// guest fetches one per git operation, so it never sees a token expire.
pub async fn git_credential(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(q): Query<GitCredentialQuery>,
    headers: HeaderMap,
) -> Result<Json<GitCredentialResponse>, ApiError> {
    let token =
        bearer(&headers).ok_or_else(|| ApiError::Unauthorized("missing forge token".into()))?;
    if !crate::api::session_auth::authorize_broker_token(&state, id, &token).await {
        return Err(ApiError::Unauthorized(
            "invalid or missing forge token".into(),
        ));
    }
    let cred = fetch_git_credential(&state, id, q.host, q.owner).await?;
    Ok(Json(GitCredentialResponse {
        username: cred.username,
        password: cred.password,
        expires_at: cred.expires_at.to_rfc3339(),
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

/// Process-backend equivalent of the one-shot guest control channel. The
/// broker token is session-scoped; the path id must also match the framed
/// request so callers cannot accidentally cross session boundaries.
pub async fn credential_control(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<ForgeRequest>,
) -> Json<ForgeResponse> {
    if id != req.session_id {
        return Json(ForgeResponse::Error {
            message: "session mismatch".into(),
        });
    }
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
    if !crate::api::session_auth::authorize_broker_token(state, req.session_id, &req.broker_token)
        .await
    {
        return ForgeResponse::Error {
            message: "invalid or missing credential broker token".into(),
        };
    }
    match req.op {
        ForgeOp::FetchCredential { host, owner } => {
            match fetch_git_credential(state, req.session_id, Some(host), owner).await {
                Ok(cred) => ForgeResponse::Credential {
                    username: cred.username,
                    password: cred.password,
                },
                Err(error) => ForgeResponse::Error {
                    message: error.into_message(),
                },
            }
        }
        ForgeOp::FetchOAuthCredential => match state.oauth.fetch_session(req.session_id).await {
            Ok((provider, version, opaque_bundle)) => ForgeResponse::OAuthCredential {
                provider,
                version,
                opaque_bundle,
            },
            Err(error) => ForgeResponse::Error {
                message: error.code().into(),
            },
        },
        ForgeOp::UpdateOAuthCredential {
            expected_version,
            opaque_bundle,
        } => match state
            .oauth
            .update_session(req.session_id, expected_version, &opaque_bundle)
            .await
        {
            Ok((provider, version, opaque_bundle)) => ForgeResponse::OAuthCredential {
                provider,
                version,
                opaque_bundle,
            },
            Err(error) => ForgeResponse::Error {
                message: error.code().into(),
            },
        },
        ForgeOp::ReportOAuthCredentialBroken {
            expected_version,
            reason,
        } => {
            // The guest picks the reason from a fixed set; cap it anyway so a
            // compromised guest cannot write unbounded text into the row.
            let reason: String = reason.chars().take(200).collect();
            match state
                .oauth
                .report_session_broken(req.session_id, expected_version, &reason)
                .await
            {
                Ok(()) => ForgeResponse::OAuthCredentialBroken,
                Err(error) => ForgeResponse::Error {
                    message: error.code().into(),
                },
            }
        }
    }
}
