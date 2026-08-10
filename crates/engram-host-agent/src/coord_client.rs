//! ADR 0013 host → coord HTTP clients.
//!
//! Functions that match the coord's HTTP endpoints
//! (`engram_coordinator::api::host_http`), all wired into `run()`'s
//! heartbeat loop / event sink / eviction-pusher:
//!
//!   - `register` — POST /api/v1/hosts/register, once at startup
//!   - `heartbeat` — POST /api/v1/hosts/:id/heartbeat, every 5s
//!   - `resolve_registry_auth` — POST /api/v1/hosts/:id/auth/resolve-registry
//!   - `harness_event` — POST /api/v1/sessions/:session_id/harness-events
//!   - `idle_eviction_candidates` — POST /api/v1/hosts/:id/idle-eviction-candidates
//!   - `claim_capture_job` (ADR 0084 P1b) — POST
//!     /api/v1/hosts/:id/capture-jobs/:job_id/claim, called from the
//!     heartbeat-ack loop for any unclaimed `capture_assignments` entry
//!
//! All share one pooled `reqwest::Client` carried by `HttpCoordClient`.
//! HTTP/1.1 keep-alive is sufficient — these are low-frequency POSTs
//! against the coord LB.

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use engram_core::{HostId, SandboxId, SessionId};
use engram_harness_proto::{
    ForgeRequest, ForgeResponse, HarnessEvent, UploadRequest, UploadResponse,
};
// ADR 0098 Phase 2: the coordinator control-plane seam + its portable
// types live in engram-host-core. `HttpCoordClient` implements
// `CoordControlPlane` below.
use engram_host_core::{
    CoordControlPlane, CoordError, LiveManifestPublishRequest, LiveManifestPublishResponse,
};
use engram_oci::{BasicCreds, OciError, RegistryAuthResolver};
use engram_protocol::heartbeat::HostCapacityReport;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::io::ReaderStream;

/// Persistent HTTP client to the coord. One per host-agent process;
/// cheap to clone (`reqwest::Client` is `Arc` internally).
#[derive(Clone)]
pub struct HttpCoordClient {
    http: reqwest::Client,
    /// `http://coord-lb:8080`. The dialer's `coordinator_endpoint`
    /// trims the `/api/hosts/connect` suffix; we keep the bare
    /// origin and append our endpoint paths.
    base_url: String,
    /// Same bearer token the WS auth header carries today. Empty
    /// string skips the header (dev coords with auth off).
    auth_token: String,
}

impl HttpCoordClient {
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
            // while heartbeats on the same `HttpCoordClient` succeeded.
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
    pub async fn register(&self, req: &RegisterRequest) -> Result<RegisterResponse, CoordError> {
        let url = self.endpoint("/hosts/register");
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "register").await
    }

    /// POST /api/v1/hosts/:id/heartbeat
    pub async fn heartbeat(
        &self,
        host_id: HostId,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, CoordError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/heartbeat"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "heartbeat").await
    }

    /// POST /api/v1/hosts/:id/auth/resolve-registry
    pub async fn resolve_registry_auth(
        &self,
        host_id: HostId,
        registry_host: &str,
    ) -> Result<ResolveRegistryAuthResponse, CoordError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/auth/resolve-registry"));
        let body = ResolveRegistryAuthRequest {
            registry_host: registry_host.to_string(),
        };
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, &body)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "resolve_registry_auth").await
    }

    /// POST /api/v1/hosts/:id/capture-jobs/:job_id/claim
    ///
    /// ADR 0084 §A: resolve the full dispatch for a `capture_assignments`
    /// entry the heartbeat-ack loop doesn't yet have running at this
    /// epoch. The coordinator validates `(host_id, job_id, epoch)`,
    /// resolves warm env + egress fresh, and returns the
    /// `CaptureJobSpec` the executor needs to actually run the capture.
    pub async fn claim_capture_job(
        &self,
        host_id: HostId,
        job_id: engram_core::types::CaptureJobId,
        epoch: i64,
    ) -> Result<engram_core::types::capture_job::CaptureJobSpec, CoordError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/capture-jobs/{job_id}/claim"));
        let body = ClaimCaptureJobRequest { epoch };
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, &body)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "claim_capture_job").await
    }

    /// POST /api/v1/sessions/:session_id/harness-events
    pub async fn harness_event(
        &self,
        session_id: SessionId,
        req: &HarnessEventRequest,
    ) -> Result<(), CoordError> {
        let url = self.endpoint(&format!("/sessions/{session_id}/harness-events"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CoordError::Http {
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
    ) -> Result<(), CoordError> {
        let url = self.endpoint(&format!("/sessions/{session_id}/integration-asset"));
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CoordError::Http {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
                what: "integration_asset",
            });
        }
        Ok(())
    }

    /// POST /api/v1/hosts/:id/sessions/:session_id/inject/refresh
    ///
    /// WS4: the egress proxy's minted inject credential (a GitHub App
    /// installation token, ~1h TTL) is nearing expiry. Ask the coord to re-mint
    /// it against the session's bound capabilities and return the fresh header +
    /// TTL. Unlike the fire-and-forget observe/harness sinks this is a
    /// request/response the proxy awaits (it substitutes the returned secret on
    /// the outbound request). A transport/HTTP failure is an `Err` — the proxy
    /// then keeps the stale secret rather than failing the guest's request.
    pub async fn refresh_inject(
        &self,
        host_id: HostId,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
    ) -> Result<RefreshInjectResponse, CoordError> {
        let url = self.endpoint(&format!(
            "/hosts/{host_id}/sessions/{session_id}/inject/refresh"
        ));
        let builder = self.http.post(&url);
        let req = RefreshInjectRequest {
            mint_source: mint_source.clone(),
            purpose: engram_core::types::integration::CredentialPurpose::api(),
        };
        let resp = self
            .auth(builder, &req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "refresh_inject").await
    }

    /// Mint a raw host-only credential for one fixed connection purpose.
    pub async fn mint_connection_credential(
        &self,
        host_id: HostId,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
        purpose: engram_core::types::integration::CredentialPurpose,
    ) -> Result<RefreshInjectResponse, CoordError> {
        let url = self.endpoint(&format!(
            "/hosts/{host_id}/sessions/{session_id}/inject/refresh"
        ));
        let req = RefreshInjectRequest {
            mint_source: mint_source.clone(),
            purpose,
        };
        let resp = self
            .auth(self.http.post(&url), &req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "mint_connection_credential").await
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
    pub async fn forge(&self, req: &ForgeRequest) -> Result<ForgeResponse, CoordError> {
        let url = self.endpoint("/hosts/forge");
        let builder = self.http.post(&url);
        let resp = self
            .auth(builder, req)
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
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
    ) -> Result<UploadResponse, CoordError>
    where
        R: tokio::io::AsyncRead + Send + 'static,
    {
        let url = self.endpoint("/hosts/upload");
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            bincode::serialize(header).map_err(|e| CoordError::Decode {
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
        let resp = builder
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        decode_json(resp, "upload_artifact").await
    }
}

/// The three decision-feeding coordinator calls the host-agent's
/// lifecycle flows make (ADR 0098 Phase 2). Behind the
/// [`CoordControlPlane`] seam so the host-internal simulator can supply an
/// adversarial scripted stub; the bodies moved here wholesale from the
/// inherent surface (no duplicate inherent copies — callers bring the
/// trait into scope).
#[async_trait]
impl CoordControlPlane for HttpCoordClient {
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
    async fn publish_live_manifest(
        &self,
        host_id: HostId,
        req: &LiveManifestPublishRequest,
    ) -> Result<LiveManifestPublishResponse, CoordError> {
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
                return Err(CoordError::Transport(e.to_string()));
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
    async fn sandbox_ownership(
        &self,
        host_id: HostId,
        session_id: engram_core::SessionId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<bool, CoordError> {
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
        let resp = builder
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CoordError::Http {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
                what: "sandbox_ownership",
            });
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        Ok(body.owned)
    }

    /// ADR 0090: the unknown-binding ownership form — "does ANY session
    /// own this sandbox on me?" Used by the teardown reconciler when the
    /// local binding table has no entry (fresh generation after a roll
    /// whose NBD rehydrate failed). Returns the owning session id so the
    /// caller can repopulate its binding table.
    async fn sandbox_owner(
        &self,
        host_id: HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<Option<engram_core::SessionId>, CoordError> {
        let url = self.endpoint(&format!("/hosts/{host_id}/sandboxes/{sandbox_id}/owner"));
        #[derive(serde::Deserialize)]
        struct Resp {
            session_id: Option<engram_core::SessionId>,
        }
        let mut builder = self.http.get(&url);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        let body: Resp = decode_json(resp, "sandbox_owner").await?;
        Ok(body.session_id)
    }
}

async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &'static str,
) -> Result<T, CoordError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(CoordError::Http {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default(),
            what,
        });
    }
    resp.json::<T>().await.map_err(|e| CoordError::Decode {
        error: e.to_string(),
        what,
    })
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
    /// ADR 0068: the probed capability vector, computed once before this
    /// register POST. `#[serde(default)]` on the coord side gives
    /// mixed-fleet interop with a pre-0068 host-agent.
    #[serde(default)]
    pub capabilities: engram_core::types::host::HostCapabilities,
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
    /// ADR 0035 amendment D2: aux bundle generations attached to each RUNNING
    /// sandbox this tick — the pin-set leg for sandboxes that exist
    /// but have not snapshotted yet. `#[serde(default)]` for the same
    /// roll-ordering reason as `current_bundles`; an old host reports
    /// nothing, which the bundle GC's grace period covers.
    #[serde(default)]
    pub sandbox_bundles: Vec<engram_core::types::sandbox::SandboxAuxBundles>,
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
    /// ADR 0073 phase 4: sandboxes with a live harness connection on
    /// this host's hub. The coordinator compares this against its own
    /// Active+bound view purely as a DISAGREEMENT ALARM (metric) — it
    /// is the demoted belt-and-braces liveness signal, never a
    /// detection input.
    #[serde(default)]
    pub harness_attached: Vec<SandboxId>,
    /// Issue #229: this host's bincode `engram_protocol::WIRE_VERSION`.
    /// The coordinator's scheduler drains a host reporting a version that
    /// differs from its own, so a non-atomic rolling deploy degrades
    /// gracefully instead of surfacing as 400 decode errors.
    #[serde(default)]
    pub wire_version: u32,
    /// ADR 0036 amendment (issue #538): true iff this host's image-prefetch
    /// supervisor is spawned. The coordinator's enable-scanner prestage
    /// stage waits only on hosts reporting this bit — a fleet with zero
    /// eligible staging hosts passes the stage vacuously.
    #[serde(default)]
    pub stages_images: bool,
    /// ADR 0068: this tick's re-probed capability vector.
    #[serde(default)]
    pub capabilities: engram_core::types::host::HostCapabilities,
    /// ADR 0084 P1b: un-acked `capture_jobs` progress/terminal reports —
    /// see [`engram_protocol::heartbeat::CheckpointAdvert`]'s twin,
    /// `capture_job::CaptureJobRecord::load_all`. Re-advertised every
    /// heartbeat until `HeartbeatResponse.acked_capture_jobs` names them.
    #[serde(default)]
    pub capture_job_reports: Vec<engram_core::types::CaptureJobReport>,
    /// ADR 0090: survivors whose NBD slot this generation quarantined —
    /// re-advertised until destroyed; the coord drives evict_local.
    #[serde(default)]
    pub quarantined_survivors: Vec<engram_protocol::heartbeat::QuarantinedSurvivor>,
    /// ADR 0091: guests whose control plane stopped answering (the
    /// checkpoint driver's 3/3-probe verdict) — `(sandbox_id,
    /// session_id)` pairs, re-advertised until a successful capture or
    /// destroy clears them. The coordinator flips the owning session
    /// Active → Unreachable.
    #[serde(default)]
    pub unreachable_guests: Vec<(SandboxId, SessionId)>,
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
    /// ADR 0036 amendment (issue #538): base-snapshot refs of images
    /// currently `prestaging` on the coordinator. The host-agent's
    /// heartbeat loop pushes the deduped union of this and `enabled_images`
    /// into the prefetch supervisor's watch channel, so a host warms an
    /// image BEFORE it's visible to session-create. `#[serde(default)]`:
    /// an old coord's ack decodes as empty (no prestage work) — exactly
    /// pre-fix behavior.
    #[serde(default)]
    pub prestage_images: Vec<engram_protocol::heartbeat::EnabledImageRef>,
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
    /// ADR 0084 P1b: `(job_id, epoch)` assignments this host should be
    /// running (or should claim, if it isn't yet). `None` = the coord's
    /// read failed (unknown — take no action); `Some` is authoritative,
    /// including `Some(vec![])`: a still-running local attempt absent
    /// from a `Some` list was reassigned away or terminally superseded,
    /// and its VM gets cancelled (fenced-off work must not keep burning
    /// capacity).
    #[serde(default)]
    pub capture_assignments: Option<Vec<engram_core::types::CaptureJobAssignment>>,
    /// ADR 0084 P1b: terminal reports from this heartbeat that landed in
    /// PG. The host deletes the matching durable capture-job records.
    #[serde(default)]
    pub acked_capture_jobs: Vec<engram_core::types::CaptureJobId>,
}

#[derive(Serialize)]
pub struct ResolveRegistryAuthRequest {
    pub registry_host: String,
}

#[derive(Deserialize)]
pub struct ResolveRegistryAuthResponse {
    pub creds: Option<RegistryCreds>,
}

#[derive(Serialize)]
pub struct ClaimCaptureJobRequest {
    pub epoch: i64,
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

#[derive(Deserialize)]
pub struct IdleEvictionCandidatesResponse {
    pub accepted: usize,
    pub failed: usize,
}

/// WS4: request body for the inject-refresh route — the source whose credential
/// the proxy needs re-minted.
#[derive(Serialize)]
pub struct RefreshInjectRequest {
    pub mint_source: engram_core::types::integration::CredentialMintSource,
    pub purpose: engram_core::types::integration::CredentialPurpose,
}

/// WS4: the coord's re-minted inject credential — the fresh rendered header
/// value the proxy substitutes, plus its new expiry (drives the next refresh).
#[derive(Deserialize)]
pub struct RefreshInjectResponse {
    pub header_name: String,
    pub secret: String,
    pub expires_at: DateTime<Utc>,
}

// ADR 0016 Phase B live-manifest-publish types + the CoordError type
// moved to `engram-host-core` (ADR 0098 Phase 2); imported at the top of
// this module.

/// `RegistryAuthResolver` impl that asks the coord for OCI creds
/// over HTTP. Replaces the WS-based `WsAuthResolver` — same
/// semantics, different transport. ADR 0013.
pub struct HttpAuthResolver {
    coord: HttpCoordClient,
    host_id: HostId,
}

impl HttpAuthResolver {
    pub fn new(coord: HttpCoordClient, host_id: HostId) -> Arc<Self> {
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
            sandbox_bundles: vec![engram_core::types::sandbox::SandboxAuxBundles {
                sandbox_id: engram_core::SandboxId::new(),
                bundles: vec![engram_core::types::sandbox::AuxBundleRef {
                    drive_id: "dyn_0".into(),
                    sha256: "ab12".into(),
                }],
            }],
            utilization: Default::default(),
            total_vcpus: 0,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: Default::default(),
            harness_attached: Vec::new(),
            capture_job_reports: Vec::new(),
            quarantined_survivors: Vec::new(),
            unreachable_guests: Vec::new(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["current_bundles"][0]["sha256"], "ff00");
        // ADR 0035 amendment D2: the per-sandbox attachment leg rides the same wire.
        assert_eq!(v["sandbox_bundles"][0]["bundles"][0]["sha256"], "ab12");
    }

    /// ADR 0084 P1b: the new capture-job HTTP-mirror fields must decode
    /// to empty/defaults for an ack from a coord that predates them
    /// (mid-rollout interop) — `#[serde(default)]` on both sides.
    #[test]
    fn heartbeat_response_capture_job_fields_default_for_old_coord() {
        let old_coord_ack = serde_json::json!({
            "server_time": "2026-07-08T00:00:00Z",
            "revoked_sessions": [],
            "enabled_images": [],
            "live_bundles": [],
        });
        let ack: HeartbeatResponse = serde_json::from_value(old_coord_ack).unwrap();
        // Absent assignments decode to None ("unknown"), NOT Some(vec![])
        // — an old coord must never look like an authoritative "cancel
        // everything you're running".
        assert!(ack.capture_assignments.is_none());
        assert!(ack.acked_capture_jobs.is_empty());
    }
}
