//! ADR 0013 host → coord HTTP clients.
//!
//! Five functions that match the five new coord HTTP endpoints
//! (`engram_coordinator::api::host_http`):
//!
//!   - `register` — POST /api/v1/hosts/register, once at startup
//!   - `heartbeat` — POST /api/v1/hosts/:id/heartbeat, every 5s
//!   - `resolve_registry_auth` — POST /api/v1/hosts/:id/auth/resolve-registry
//!   - `harness_event` — POST /api/v1/sessions/:session_id/harness-events
//!   - `idle_eviction_candidates` — POST /api/v1/hosts/:id/idle-eviction-candidates
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
use base64::Engine as _;
use chrono::{DateTime, Utc};
use engram_core::{HostId, SandboxId, SessionId};
use engram_harness_proto::{
    ForgeRequest, ForgeResponse, HarnessEvent, UploadRequest, UploadResponse,
};
use engram_oci::{BasicCreds, OciError, RegistryAuthResolver};
use engram_protocol::heartbeat::{HostCapacityReport, LocalSnapshotReport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::io::ReaderStream;

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

    /// All coord HTTP routes live under the `/api/v1` prefix (the SPA
    /// owns the root path namespace). `path` is the route below that
    /// prefix, e.g. `/hosts/register` or `/sessions/:id/harness-events`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
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

    /// POST /api/v1/hosts/register
    pub async fn register(
        &self,
        req: &RegisterRequest,
    ) -> Result<RegisterResponse, CoordClientError> {
        let url = self.endpoint("/hosts/register");
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "register").await
    }

    /// POST /api/v1/hosts/:id/heartbeat
    pub async fn heartbeat(
        &self,
        host_id: HostId,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, CoordClientError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/heartbeat"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "heartbeat").await
    }

    /// POST /api/v1/hosts/:id/auth/resolve-registry
    pub async fn resolve_registry_auth(
        &self,
        host_id: HostId,
        registry_host: &str,
    ) -> Result<ResolveRegistryAuthResponse, CoordClientError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/auth/resolve-registry"));
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

    /// POST /api/v1/sessions/:session_id/harness-events
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

    /// POST /api/v1/sessions/:session_id/integration-asset
    ///
    /// ADR 0056 Phase 4: the egress proxy observed a response on a marked
    /// endpoint and built an asset. The host-agent forwards it to the coord —
    /// which appends it as an `IntegrationAsset` session event — mirroring the
    /// harness-event path. Best-effort: a transport/HTTP failure is logged by
    /// the caller, not retried (at-least-once observation, ADR 0028).
    pub async fn integration_asset(
        &self,
        session_id: SessionId,
        req: &IntegrationAssetReport,
    ) -> Result<(), CoordClientError> {
        let url = self.endpoint(&format!("/sessions/{session_id}/integration-asset"));
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
                what: "integration_asset",
            });
        }
        Ok(())
    }

    /// POST /api/v1/hosts/forge
    ///
    /// ADR 0023 split-mode forge forwarding. The host-agent reads the
    /// in-guest `ForgeRequest` off the forge vsock stream and proxies it
    /// to the coord (which holds the `GitForge` + broker map), then writes
    /// the returned `ForgeResponse` back to the guest. A transport/HTTP
    /// failure surfaces as an `Err`; a forge-level failure (bad token,
    /// API error) comes back inside `ForgeResponse::Error` with a 200, so
    /// the guest helper always gets a usable reply.
    pub async fn forge(&self, req: &ForgeRequest) -> Result<ForgeResponse, CoordClientError> {
        let url = self.endpoint("/hosts/forge");
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(CoordClientError::Transport)?;
        decode_json(resp, "forge").await
    }

    /// POST /api/v1/hosts/upload
    ///
    /// ADR 0026 split-mode artifact forwarding. The host-agent reads the
    /// in-guest `UploadRequest` header off the upload vsock stream, then
    /// streams the raw file body straight through to the coord (which
    /// holds BlobStorage + the broker map) **without buffering**, and
    /// writes the returned `UploadResponse` back to the guest. The header
    /// rides a base64'd bincode `X-Engram-Upload` request header — keeps
    /// the broker token out of the URL/query log and survives a unicode
    /// caption; the body is the raw file bytes. The coord always replies
    /// 200 with an `UploadResponse` (upload-level failures are
    /// `UploadResponse::Error`), so the guest helper gets a usable reply.
    pub async fn upload_artifact<R>(
        &self,
        header: &UploadRequest,
        body: R,
    ) -> Result<UploadResponse, CoordClientError>
    where
        R: tokio::io::AsyncRead + Send + 'static,
    {
        let url = self.endpoint("/hosts/upload");
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            bincode::serialize(header).map_err(|e| CoordClientError::Decode {
                error: e.to_string(),
                what: "upload_artifact header encode",
            })?,
        );
        let stream_body = reqwest::Body::wrap_stream(ReaderStream::new(Box::pin(body)));
        let mut builder = self
            .http
            .post(&url)
            .header("X-Engram-Upload", encoded)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            // A multi-hundred-MB video can take a while to stream through;
            // override the shared client's 30s default for this lane.
            .timeout(Duration::from_secs(600))
            .body(stream_body);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }
        let resp = builder.send().await.map_err(CoordClientError::Transport)?;
        decode_json(resp, "upload_artifact").await
    }

    /// POST /api/v1/hosts/:id/live-manifest
    ///
    /// ADR 0016 Phase B: tells coord that the host just flushed
    /// `(sandbox_id, manifest_ref)` so `sessions.live_disk_manifest_*`
    /// can be updated. Coord's UPDATE is gated on `sandbox_id` match,
    /// so a stale publish (sandbox destroyed/rebound) returns
    /// `LiveManifestPublishOutcome::Stale` and the host should NOT
    /// retry; staleness is structural, not transient.
    ///
    /// Publishes are tiny (sub-100-byte payload) and fast; the
    /// shared client's 30s default timeout is plenty. No per-request
    /// override here, unlike the eviction lane in A.1.5a.
    pub async fn publish_live_manifest(
        &self,
        host_id: HostId,
        req: &LiveManifestPublishRequest,
    ) -> Result<LiveManifestPublishResponse, CoordClientError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/live-manifest"));
        let builder = self.http.post(&url);
        let resp = match self.auth(builder, req).send().await {
            Ok(r) => r,
            Err(e) => {
                // ADR 0016 §A.1.3 pattern: log transport-error
                // breakdown for ops diagnosis. Don't escalate to
                // warn — a single stale-pool error is benign; the
                // drain task retries on the next wake.
                tracing::debug!(
                    %host_id,
                    is_connect = e.is_connect(),
                    is_timeout = e.is_timeout(),
                    is_request = e.is_request(),
                    is_body = e.is_body(),
                    error = %e,
                    "publish_live_manifest transport error",
                );
                return Err(CoordClientError::Transport(e));
            }
        };
        decode_json(resp, "publish_live_manifest").await
    }

    /// POST /api/v1/hosts/:id/idle-eviction-candidates
    /// ADR 0045 C1: the export-TTL ownership check. `Ok(true)` = the
    /// coordinator still binds this sandbox to the session (the move
    /// never landed — abort the export, un-pause in place);
    /// `Ok(false)` = ownership moved on (destroy the stale frozen
    /// source); `Err` = coordinator unreachable (stay paused, retry).
    pub async fn sandbox_ownership(
        &self,
        host_id: HostId,
        session_id: engram_core::SessionId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<bool, CoordClientError> {
        let url = self.endpoint(&format!(
            "/hosts/{host_id}/sessions/{session_id}/sandboxes/{sandbox_id}/ownership"
        ));
        #[derive(serde::Deserialize)]
        struct Resp {
            owned: bool,
        }
        let mut builder = self.http.get(&url);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }
        let resp = builder.send().await.map_err(CoordClientError::Transport)?;
        if !resp.status().is_success() {
            return Err(CoordClientError::Http {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
                what: "sandbox_ownership",
            });
        }
        let body: Resp = resp.json().await.map_err(CoordClientError::Transport)?;
        Ok(body.owned)
    }

    pub async fn push_idle_eviction_candidates(
        &self,
        host_id: HostId,
        candidates: Vec<IdleCandidate>,
    ) -> Result<IdleEvictionCandidatesResponse, CoordClientError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/idle-eviction-candidates"));
        let body = IdleEvictionCandidatesRequest { candidates };
        // ADR 0034 retired ADR 0016 §A.1.5a's per-request 120s
        // override: the handler is a fast Active→Evicting nomination
        // now (one PG UPDATE per candidate) — the snapshot pipeline
        // runs on the coord's eviction scanner, detached from this
        // request. The shared client's 30s default is ample. (The
        // 120s override was also the cancellation fuse in the
        // 0782bea5 incident: the timeout aborted the then-inline
        // pipeline mid-snapshot.)
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
    /// ADR 0016 Phase B commit 7: rehydration list. One entry per
    /// Active session bound to this host with a chunked-disk
    /// manifest PG knows about. Empty on first registration / no
    /// survivors. Coord-side: see
    /// `engram_coordinator::api::host_http::RehydrateSandboxRef`.
    #[serde(default)]
    pub rehydrate_sandboxes: Vec<RehydrateSandboxRef>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RehydrateSandboxRef {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub disk_manifest_id: Option<uuid::Uuid>,
    pub disk_manifest_version: Option<u64>,
}

#[derive(Serialize)]
pub struct HeartbeatRequest {
    pub capacity: HostCapacityReport,
    #[serde(default)]
    pub local_snapshots: Vec<LocalSnapshotReport>,
    #[serde(default)]
    pub running_sandboxes: Vec<SandboxId>,
    /// Issue #215: `false` iff `backend.list()` failed this tick, so
    /// `running_sandboxes` carries no usable signal and the coord must
    /// skip its ADR 0009 reconcile rather than strike every session.
    /// (Serialize-only struct; the coord's mirror supplies the
    /// deserialize-side default for mixed-version interop.)
    pub running_sandboxes_known: bool,
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
    /// ADR 0035: the bake stamp's `drive_id` → sha256 set — which
    /// bundle generation this host image carries as *current*. Ops
    /// visibility (fleet skew mid-roll) + a defensive member of the
    /// coord's bundle-GC pin set.
    #[serde(default)]
    pub current_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// ADR 0028 Fix A: un-acked durable checkpoint records — see
    /// [`engram_protocol::heartbeat::CheckpointAdvert`]. Re-advertised
    /// every heartbeat until the ack's `acked_checkpoints` clears
    /// them.
    #[serde(default)]
    pub checkpoints: Vec<engram_protocol::heartbeat::CheckpointAdvert>,
    /// Observed disk/mem/cpu utilization this tick — rendered by the
    /// operator fleet view. `#[serde(default)]` for interop with a
    /// coord that predates the field.
    #[serde(default)]
    pub utilization: engram_core::types::host::HostUtilization,
    /// ADR 0048: this host's core count — the basis of the
    /// coordinator's CPU packing budget (`total_vcpus × overcommit`).
    /// `#[serde(default)]` for interop both ways (0 = unknown).
    #[serde(default)]
    pub total_vcpus: u32,
    /// Issue #229: this host's bincode `engram_protocol::WIRE_VERSION`.
    /// The coordinator's scheduler drains a host reporting a version that
    /// differs from its own, so a non-atomic rolling deploy degrades
    /// gracefully instead of surfacing as 400 decode errors.
    #[serde(default)]
    pub wire_version: u32,
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
    /// ADR 0035 §5: the coord's bundle pin set (every generation some
    /// snapshot row references). The host's bundle supervisor
    /// prefetches missing pinned generations and sweeps staged files
    /// outside pin-set ∪ bake-stamp. Deliberately NOT
    /// `serde(default)`: an old coord's ack must fail decode (heartbeat
    /// retries until the coord roll completes) rather than read as an
    /// empty pin set and sweep generations resumes still need.
    pub live_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// ADR 0028 Fix A: adverts from this heartbeat the coord recorded
    /// into PG. The host deletes the matching durable record files.
    #[serde(default)]
    pub acked_checkpoints: Vec<engram_core::types::SnapshotId>,
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

/// ADR 0056 Phase 4: a proxy-observed integration asset, forwarded to the
/// coord's `/sessions/:id/integration-asset` ingest. `surface` is opaque here
/// ("action" | "asset"); the coord maps it to its `AssetSurface`.
#[derive(Serialize)]
pub struct IntegrationAssetReport {
    pub provider: String,
    pub asset_kind: String,
    pub surface: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetchable_url: Option<String>,
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

// ---- ADR 0016 Phase B: live disk manifest publish ----

#[derive(Serialize, Deserialize)]
pub struct LiveManifestPublishRequest {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub manifest_id: uuid::Uuid,
    pub manifest_version: u64,
}

#[derive(Serialize, Deserialize)]
pub struct LiveManifestPublishResponse {
    /// `applied` → the UPDATE matched the row and chunk_generation
    /// ticked. `stale` → `sessions.sandbox_id != publish.sandbox_id`
    /// (destroyed or rebound); host should NOT retry.
    pub outcome: LiveManifestPublishOutcome,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum LiveManifestPublishOutcome {
    Applied,
    Stale,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0035 §5: `live_bundles` is deliberately NOT
    /// `serde(default)`. An old coord's ack (mid-deploy) must fail
    /// decode — the heartbeat retries next tick — because treating
    /// "field absent" as "empty pin set" would instruct the host's
    /// bundle supervisor to sweep generations resumes still need.
    /// This test is the contract; if you're here because you added
    /// `#[serde(default)]` to make a test pass, that's the bug.
    #[test]
    fn heartbeat_response_rejects_missing_live_bundles() {
        let old_coord_ack = serde_json::json!({
            "server_time": "2026-06-03T00:00:00Z",
            "revoked_sessions": [],
            "enabled_images": [],
        });
        let res = serde_json::from_value::<HeartbeatResponse>(old_coord_ack);
        assert!(res.is_err(), "ack without live_bundles must fail decode");

        let new_coord_ack = serde_json::json!({
            "server_time": "2026-06-03T00:00:00Z",
            "revoked_sessions": [],
            "enabled_images": [],
            "live_bundles": [{"drive_id": "skills", "sha256": "ab12"}],
        });
        let ack: HeartbeatResponse = serde_json::from_value(new_coord_ack).unwrap();
        assert_eq!(ack.live_bundles.len(), 1);
        assert_eq!(ack.live_bundles[0].drive_id, "skills");
    }

    /// The request side IS `serde(default)`: coord rolls before the
    /// host-agent pod restart, so a new coord must accept old hosts'
    /// heartbeats (they simply report no current bundles).
    #[test]
    fn heartbeat_request_current_bundles_serializes() {
        let req = HeartbeatRequest {
            capacity: HostCapacityReport::default(),
            local_snapshots: vec![],
            running_sandboxes: vec![],
            running_sandboxes_known: true,
            draining: false,
            host_addr: None,
            ready_images: vec![],
            checkpoints: vec![],
            current_bundles: vec![engram_core::types::sandbox::AuxBundleRef {
                drive_id: "skills".into(),
                sha256: "ff00".into(),
            }],
            utilization: Default::default(),
            total_vcpus: 0,
            wire_version: engram_protocol::WIRE_VERSION,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["current_bundles"][0]["sha256"], "ff00");
    }
}
