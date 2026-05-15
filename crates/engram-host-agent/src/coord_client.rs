//! ADR 0013 host → coord HTTP clients.
//!
//! Five functions that match the five new coord HTTP endpoints
//! (`engram_coordinator::api::host_http`):
//!
//!   - `register` — POST /api/hosts/register, once at startup
//!   - `heartbeat` — POST /api/hosts/:id/heartbeat, every 5s
//!   - `resolve_registry_auth` — POST /api/hosts/:id/auth/resolve-registry
//!   - `harness_event` — POST /sessions/:session_id/harness-events
//!   - `idle_eviction_candidates` — POST /api/hosts/:id/idle-eviction-candidates
//!
//! The functions exist standalone (dark) in this commit so the
//! cutover commit can wire them into the dialer / event sink /
//! eviction-pusher in one move. Today only `register` is called
//! from `run()`.
//!
//! All five share one pooled `reqwest::Client` carried by
//! `CoordClient`. HTTP/1.1 keep-alive is sufficient — these are
//! low-frequency POSTs against the coord LB.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::{HostId, SandboxId, SessionId};
use engram_harness_proto::HarnessEvent;
use engram_oci::{BasicCreds, OciError, RegistryAuthResolver};
use engram_protocol::heartbeat::{HostCapacityReport, LocalSnapshotReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Persistent HTTP client to the coord. One per host-agent process;
/// cheap to clone (`reqwest::Client` is `Arc` internally).
#[derive(Clone)]
pub struct CoordClient {
    http: reqwest::Client,
    /// `http://coord-lb:8080`. The dialer's `coordinator_endpoint`
    /// trims the `/api/hosts/connect` suffix; we keep the bare
    /// origin and append our endpoint paths.
    base_url: String,
    /// Same bearer token the WS auth header carries today. Empty
    /// string skips the header (dev coords with auth off).
    auth_token: String,
}

impl CoordClient {
    pub fn new(coord_url: String, auth_token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder must not fail with default config");
        Self {
            http,
            base_url: trim_ws_suffix(&coord_url),
            auth_token: auth_token.unwrap_or_default(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth<R: serde::Serialize>(
        &self,
        builder: reqwest::RequestBuilder,
        body: &R,
    ) -> reqwest::RequestBuilder {
        let b = builder.json(body);
        if !self.auth_token.is_empty() {
            b.bearer_auth(&self.auth_token)
        } else {
            b
        }
    }

    /// POST /api/hosts/register
    pub async fn register(
        &self,
        req: &RegisterRequest,
    ) -> Result<RegisterResponse, CoordClientError> {
        let url = self.endpoint("/api/hosts/register");
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "register").await
    }

    /// POST /api/hosts/:id/heartbeat
    pub async fn heartbeat(
        &self,
        host_id: HostId,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, CoordClientError> {
        let url = self.endpoint(&format!("/api/hosts/{host_id}/heartbeat"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "heartbeat").await
    }

    /// POST /api/hosts/:id/auth/resolve-registry
    pub async fn resolve_registry_auth(
        &self,
        host_id: HostId,
        registry_host: &str,
    ) -> Result<ResolveRegistryAuthResponse, CoordClientError> {
        let url = self.endpoint(&format!("/api/hosts/{host_id}/auth/resolve-registry"));
        let body = ResolveRegistryAuthRequest {
            registry_host: registry_host.to_string(),
        };
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, &body)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "resolve_registry_auth").await
    }

    /// POST /sessions/:session_id/harness-events
    pub async fn harness_event(
        &self,
        session_id: SessionId,
        req: &HarnessEventRequest,
    ) -> Result<(), CoordClientError> {
        let url = self.endpoint(&format!("/sessions/{session_id}/harness-events"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        if !resp.status().is_success() {
            return Err(CoordClientError::Http {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
                what: "harness_event",
            });
        }
        Ok(())
    }

    /// POST /api/hosts/:id/idle-eviction-candidates
    pub async fn push_idle_eviction_candidates(
        &self,
        host_id: HostId,
        candidates: Vec<IdleCandidate>,
    ) -> Result<IdleEvictionCandidatesResponse, CoordClientError> {
        let url = self.endpoint(&format!("/api/hosts/{host_id}/idle-eviction-candidates"));
        let body = IdleEvictionCandidatesRequest { candidates };
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, &body)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "idle_eviction_candidates").await
    }
}

fn trim_ws_suffix(coord_url: &str) -> String {
    // Tolerate callers that pass the WS-flavored URL today
    // (`http://coord:8080/api/hosts/connect`) so the cutover commit
    // can swap dialer code without rewriting config.
    let trimmed = coord_url.trim_end_matches('/');
    if let Some(stripped) = trimmed.strip_suffix("/api/hosts/connect") {
        stripped.to_string()
    } else {
        trimmed.to_string()
    }
}

async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &'static str,
) -> Result<T, CoordClientError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(CoordClientError::Http {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default(),
            what,
        });
    }
    resp.json::<T>()
        .await
        .map_err(|e| CoordClientError::Decode {
            error: e.to_string(),
            what,
        })
}

#[derive(Debug)]
pub enum CoordClientError {
    Transport(reqwest::Error),
    Http {
        status: u16,
        body: String,
        what: &'static str,
    },
    Decode {
        error: String,
        what: &'static str,
    },
}

impl std::fmt::Display for CoordClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport error: {e}"),
            Self::Http { status, body, what } => {
                write!(f, "{what} returned HTTP {status}: {body}")
            }
            Self::Decode { error, what } => write!(f, "{what} JSON decode failed: {error}"),
        }
    }
}

impl std::error::Error for CoordClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            _ => None,
        }
    }
}

// ---- payload types (mirror coord-side host_http.rs) ----

#[derive(Serialize)]
pub struct RegisterRequest {
    pub host_id: HostId,
    pub hostname: String,
    pub host_addr: String,
    pub agent_version: String,
    pub wire_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_metadata: Option<engram_core::types::host::HostMetadata>,
}

#[derive(Deserialize)]
pub struct RegisterResponse {
    pub server_time: DateTime<Utc>,
    pub coord_wire_version: u32,
}

#[derive(Serialize)]
pub struct HeartbeatRequest {
    pub capacity: HostCapacityReport,
    #[serde(default)]
    pub local_snapshots: Vec<LocalSnapshotReport>,
    #[serde(default)]
    pub running_sandboxes: Vec<SandboxId>,
    #[serde(default)]
    pub draining: bool,
}

#[derive(Deserialize)]
pub struct HeartbeatResponse {
    pub server_time: DateTime<Utc>,
    pub revoked_sessions: Vec<SessionId>,
}

#[derive(Serialize)]
pub struct ResolveRegistryAuthRequest {
    pub registry_host: String,
}

#[derive(Deserialize)]
pub struct ResolveRegistryAuthResponse {
    pub creds: Option<RegistryCreds>,
}

#[derive(Deserialize)]
pub struct RegistryCreds {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct HarnessEventRequest {
    pub sandbox_id: SandboxId,
    pub event: HarnessEvent,
    pub at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct IdleCandidate {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct IdleEvictionCandidatesRequest {
    pub candidates: Vec<IdleCandidate>,
}

#[derive(Deserialize)]
pub struct IdleEvictionCandidatesResponse {
    pub accepted: usize,
    pub failed: usize,
}

/// `RegistryAuthResolver` impl that asks the coord for OCI creds
/// over HTTP. Replaces the WS-based `WsAuthResolver` — same
/// semantics, different transport. ADR 0013.
pub struct HttpAuthResolver {
    coord: CoordClient,
    host_id: HostId,
}

impl HttpAuthResolver {
    pub fn new(coord: CoordClient, host_id: HostId) -> Arc<Self> {
        Arc::new(Self { coord, host_id })
    }
}

#[async_trait]
impl RegistryAuthResolver for HttpAuthResolver {
    async fn resolve(&self, registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        match self
            .coord
            .resolve_registry_auth(self.host_id, registry_host)
            .await
        {
            Ok(resp) => Ok(resp.creds.map(|c| BasicCreds {
                username: c.username,
                password: c.password,
            })),
            Err(e) => Err(OciError::Distribution(format!("HttpAuthResolver: {e}"))),
        }
    }
}
