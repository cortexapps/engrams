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
    /// `coord_url` must be `http://host[:port]` or `https://host[:port]`.
    /// ADR 0013 retired the WS dialer; ADR 0016 §A.1.4 retired the
    /// `ws://`/`wss://` compat shim once all TF configs migrated. A
    /// non-HTTP scheme here is a deployment misconfiguration; the
    /// `reqwest` builder downstream will reject it with a clear
    /// "builder error for url" at the first send.
    pub fn new(coord_url: String, auth_token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            // ADR 0016 §A.1.3: 10s pool-idle timeout (was 90s).
            //
            // Why tighten it. The coord LB front-end IP (10.10.0.2) is
            // stable but the back-end pod rolls on every helm-deploy
            // (multiple times per merge to main). GCP internal HTTP
            // LB resets connections to drained back-ends, so any
            // pooled TCP connection bound to a now-gone pod fails
            // with `connection reset` on next reuse — surfaced to
            // reqwest as a transport error.
            //
            // The lanes here have very different cadences:
            //   - heartbeat: every 5s → pooled connection stays warm
            //   - harness_event: per in-VM event, can be sub-second
            //   - idle_eviction_candidates: every 30s+ → pool entry
            //     can be 30-89s old → high stale-rate
            //
            // 10s pool idle keeps heartbeat + harness_event efficient
            // (one cached TCP connection across each session of
            // activity) but ensures slower lanes always pick a fresh
            // connection. Per-request overhead is one TCP connect
            // (~5-10ms over the internal LB) — negligible against
            // the workload.
            //
            // Surfaced on 2026-05-23 during the COW diagnostic
            // spot-check on session 96392fd3: the host's
            // idle-eviction POSTs failed with `transport error`
            // while heartbeats on the same `CoordClient` succeeded.
            // Diagnosis: stale pool connection, exactly this class.
            .pool_idle_timeout(Some(Duration::from_secs(10)))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder must not fail with default config");
        Self {
            http,
            base_url: coord_url.trim_end_matches('/').to_string(),
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
        let resp = match self.auth(builder, &body).send().await {
            Ok(r) => r,
            Err(e) => {
                // ADR 0016 §A.1.3: structured transport-error log
                // so we can attribute pool-staleness vs DNS vs
                // genuine connect-refused. The caller's existing
                // "POST failed; retrying next tick" line in
                // `lib.rs::eviction_task` loses this detail.
                tracing::debug!(
                    %host_id,
                    is_connect = e.is_connect(),
                    is_timeout = e.is_timeout(),
                    is_request = e.is_request(),
                    is_body = e.is_body(),
                    error = %e,
                    "idle-eviction transport error (reqwest); \
                     `is_connect=true` usually means stale pool conn",
                );
                return Err(CoordClientError::Transport(e));
            }
        };
        decode_json(resp, "idle_eviction_candidates").await
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
    /// ADR 0013: gRPC advertise URL the host registered with. Sent on
    /// every heartbeat so any coord pod can self-heal its in-memory
    /// registry from heartbeat traffic alone (coord rolling restart
    /// would otherwise leave the new pod empty until each host's
    /// next agent restart). `None` when the agent has no addr to
    /// advertise (`--grpc-listen-addr disabled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_addr: Option<String>,
    /// ADR 0015 M5: manifest digests of every image this host has
    /// fully prefetched to local NVMe. The coord scheduler's
    /// `pick_for_session` filter gates host selection on
    /// `ready_images.contains(digest)`.
    #[serde(default)]
    pub ready_images: Vec<engram_protocol::heartbeat::ManifestDigest>,
}

#[derive(Deserialize)]
pub struct HeartbeatResponse {
    pub server_time: DateTime<Utc>,
    pub revoked_sessions: Vec<SessionId>,
    /// ADR 0015 M5: coord's authoritative `enabled_images` set. The
    /// host's prefetch supervisor diffs this against the local NVMe
    /// chunk cache and drives chunk pulls for any image not yet
    /// ready.
    #[serde(default)]
    pub enabled_images: Vec<engram_protocol::heartbeat::EnabledImageRef>,
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
