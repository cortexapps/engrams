//! ADR 0013 host → coord HTTP endpoints (dark in this commit).
//!
//! Replaces the host-initiated bits of the WS protocol with plain
//! HTTP/JSON POSTs that any coord pod can serve. The host-agent
//! doesn't call these yet — that wiring lands in the cutover
//! commit. Routing is live so a `curl` against a coord pod
//! exercises the endpoint today.
//!
//! Five endpoints:
//!   - `POST /api/hosts/register` — once per host-agent startup
//!   - `POST /api/hosts/:id/heartbeat` — every 5s
//!   - `POST /api/hosts/:id/auth/resolve-registry` — host requests
//!     OCI creds during image pull
//!   - `POST /api/sessions/:session_id/harness-events` — host
//!     forwards adapter events one POST at a time
//!   - `POST /api/hosts/:id/idle-eviction-candidates` — host pushes
//!     idle-eviction candidates (ADR 0011 follow-up #2)
//!
//! Each handler is a thin shim over existing logic — the WS path's
//! supervisor loop, the existing `AuthRequestHandler::handle` body,
//! and `idle_evictor::evict_idle_session` stay authoritative.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::types::host::{HostCapacity, HostMetadata, HostRecord, HostStatus};
use engram_core::{HostId, SandboxId, SessionId};
use engram_harness_proto::HarnessEvent;
use engram_protocol::heartbeat::{
    EnabledImageRef, HostCapacityReport, LocalSnapshotReport, ManifestDigest,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::host_registry::HostState;
use crate::idle_evictor;
use crate::state::SharedState;

// ---- POST /api/hosts/register ----

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub host_id: HostId,
    pub hostname: String,
    /// gRPC dial address (e.g. `http://10.10.0.42:9101`). The coord
    /// pod that fields this POST persists it to `hosts.host_addr`
    /// and (once wired) warms its `GrpcHostPool` entry so the next
    /// session-create RPC doesn't pay cold-dial latency.
    pub host_addr: String,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub wire_version: u32,
    #[serde(default)]
    pub cloud_metadata: Option<HostMetadata>,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub server_time: DateTime<Utc>,
    /// Echo of the wire version the coord understands. A host can
    /// compare and refuse to start if there's a mismatch. We don't
    /// gate registration on it today — the bincode-wire-version
    /// invariant that existed on the WS path didn't survive into
    /// gRPC, since gRPC carries its own schema.
    pub coord_wire_version: u32,
    /// ADR 0015 M5: the current enabled-images set, same shape the
    /// heartbeat-ack carries. Piggy-backing on register lets the
    /// host start its prefetch loop immediately, shrinking the
    /// first-ready window from `heartbeat_interval + prefetch_time`
    /// to just `prefetch_time`.
    #[serde(default)]
    pub enabled_images: Vec<EnabledImageRef>,
    /// ADR 0016 Phase B commit 7: rehydration source. One record
    /// per Active session PG already has bound to this host with
    /// a chunked-disk manifest. Host iterates this on startup,
    /// rebuilds `ChunkedDiskBackend` from the effective manifest,
    /// spawns the NBD daemon + FlushScheduler. Closes the
    /// host-agent-restart-loses-COW-tracking gap the ADR's
    /// commit-0 design called out.
    ///
    /// Empty when:
    /// - The host's first registration (no Active sessions yet).
    /// - The host's restart but no Active sessions in PG were
    ///   bound (eviction sweep ran while the host was down).
    /// - The coord lookup failed (we log + emit empty so the host
    ///   continues; sandboxes recover via the M4 evac path).
    #[serde(default)]
    pub rehydrate_sandboxes: Vec<RehydrateSandboxRef>,
}

/// ADR 0016 Phase B commit 7: one entry in the rehydration list
/// the host iterates at startup. Wire format mirrors the columns
/// `MetadataStore::list_active_sandboxes_on_host_with_disk_manifest`
/// returns; the host's startup hook rebuilds chunked-disk tracking
/// from these.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RehydrateSandboxRef {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    /// The effective disk manifest the host should rebuild from
    /// (newer of `sessions.live_disk_manifest_*` and the latest
    /// recoverable `snapshots.disk_manifest`). `None` means there's
    /// no chunked-disk lineage to rebuild — host skips this row.
    pub disk_manifest_id: Option<uuid::Uuid>,
    pub disk_manifest_version: Option<u64>,
}

pub async fn register(
    State(state): State<SharedState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    if req.host_addr.is_empty() {
        return Err(ApiError::BadRequest(
            "host_addr is required for registration".into(),
        ));
    }
    if req.wire_version != 0 && req.wire_version != engram_protocol::WIRE_VERSION {
        tracing::warn!(
            host_id = %req.host_id,
            host_wire_version = req.wire_version,
            coord_wire_version = engram_protocol::WIRE_VERSION,
            "wire_version mismatch on host register; tolerating (gRPC carries its own schema)",
        );
    }
    let record = HostRecord {
        id: req.host_id,
        hostname: req.hostname,
        cloud_metadata: req.cloud_metadata.unwrap_or_default(),
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: 0,
            used_mib: 0,
            running_sandboxes: 0,
        },
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: Some(req.host_addr.clone()),
    };
    state.services.meta.upsert_host(record).await?;

    // ADR 0013: warm the gRPC pool entry + register the host's
    // GrpcHostClient with `HostRegistry` so subsequent
    // dispatch lands on the right gRPC channel. Pool.warm fires
    // a Ping to force the TCP+H2 handshake; the channel is
    // already inserted into the pool's DashMap regardless of
    // ping outcome (lazy connect tolerates a brief unreachable
    // window — first real RPC retries).
    state
        .services
        .host_pool
        .warm(req.host_id, req.host_addr.clone())
        .await?;
    let grpc_client = state.services.host_pool.get(req.host_id)?;
    let backend: std::sync::Arc<dyn engram_core::traits::HostClient> =
        std::sync::Arc::new(grpc_client);
    state.host_registry.register(req.host_id, backend);

    // ADR 0015 M5: hand the host the current enabled-images set so
    // it can start its prefetch loop without waiting for the first
    // heartbeat tick. Failure here is non-fatal — the heartbeat
    // path re-delivers on the next tick.
    let enabled_images = match state.services.meta.list_enabled_images().await {
        Ok(rows) => enabled_image_refs_from_rows(rows),
        Err(e) => {
            tracing::debug!(
                host_id = %req.host_id,
                error = %e,
                "list_enabled_images failed at register; host will pick them up on next heartbeat",
            );
            Vec::new()
        }
    };

    // ADR 0016 Phase B commit 7: rehydration list. PG already
    // knows which Active sessions are bound to this host (a host-
    // agent restart drops `nbd_sandboxes` but PG persists the
    // session→sandbox→host binding). Hand the list back so the
    // host can rebuild `ChunkedDiskBackend` + spawn the
    // FlushScheduler for each, restoring continuous-flush
    // coverage. Failure non-fatal: a host that doesn't get the
    // list runs blind for the survivors (same shape as pre-Phase-
    // B behaviour); operator can manually re-register or evac.
    let rehydrate_sandboxes = match state
        .services
        .meta
        .list_active_sandboxes_on_host_with_disk_manifest(req.host_id)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(session_id, sandbox_id, manifest)| RehydrateSandboxRef {
                session_id,
                sandbox_id,
                disk_manifest_id: manifest.as_ref().map(|m| m.manifest_id),
                disk_manifest_version: manifest.as_ref().map(|m| m.version),
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                host_id = %req.host_id,
                error = %e,
                "rehydrate-list query failed at register; host will run blind for survivors",
            );
            Vec::new()
        }
    };

    tracing::info!(
        host_id = %req.host_id,
        host_addr = %req.host_addr,
        agent_version = %req.agent_version,
        enabled_images = enabled_images.len(),
        rehydrate_sandboxes = rehydrate_sandboxes.len(),
        "host registered via /api/hosts/register",
    );

    Ok(Json(RegisterResponse {
        server_time: Utc::now(),
        coord_wire_version: engram_protocol::WIRE_VERSION,
        enabled_images,
        rehydrate_sandboxes,
    }))
}

// ---- POST /api/hosts/:id/heartbeat ----

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    /// Wire mirror of `engram_protocol::heartbeat::Heartbeat` minus
    /// the redundant `host_id` (which travels in the URL) and
    /// `sent_at` (which the coord doesn't read today).
    pub capacity: HostCapacityReport,
    #[serde(default)]
    pub local_snapshots: Vec<LocalSnapshotReport>,
    #[serde(default)]
    pub running_sandboxes: Vec<SandboxId>,
    #[serde(default)]
    pub draining: bool,
    /// ADR 0013: host's gRPC advertise address. Carried on every
    /// heartbeat (not just register) so any coord pod can self-heal
    /// its in-memory registry from heartbeat traffic alone — even
    /// after a rolling restart where the new pod has no prior state
    /// and the host won't re-register until the next agent restart.
    /// Optional for backward-compat with pre-fix host-agents during
    /// rollout; once they've all redeployed, presence is the norm.
    #[serde(default)]
    pub host_addr: Option<String>,
    /// ADR 0015 M5: manifest digests of images this host has fully
    /// prefetched to local NVMe. The scheduler gates session
    /// placement on `ready_images.contains(&digest)`.
    #[serde(default)]
    pub ready_images: Vec<ManifestDigest>,
    /// ADR 0018 Phase B: sandbox IDs whose backing `/dev/nbdN` has
    /// failed health probes. Coord's heartbeat handler (commit 5)
    /// fires the evacuation primitive against each entry.
    /// `#[serde(default)]` for back-compat with pre-Phase-B hosts.
    #[serde(default)]
    pub nbd_unhealthy: Vec<SandboxId>,
}

#[derive(Serialize)]
pub struct HeartbeatResponse {
    pub server_time: DateTime<Utc>,
    /// Sessions the coord is revoking from this host. Today this is
    /// always empty (revocation lands via a separate path); kept on
    /// the wire so a future revocation flow doesn't need a new
    /// endpoint.
    pub revoked_sessions: Vec<SessionId>,
    /// ADR 0015 M5: coord's authoritative enabled-images set. The
    /// host's prefetch supervisor diffs this against its local
    /// `ready_images` and pulls missing chunks.
    #[serde(default)]
    pub enabled_images: Vec<EnabledImageRef>,
}

pub async fn heartbeat(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
    Json(hb): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    // ADR 0013 self-heal: ensure the host is in this pod's
    // in-memory registry + the gRPC pool, even if we never saw
    // the original `/api/hosts/register` POST (e.g. coord pod was
    // just rolled by helm-deploy; the host already registered
    // against the previous pod and won't re-register until its
    // next agent restart). Heartbeat carries `host_addr` so we
    // don't need a PG lookup; the warm + register are idempotent
    // on the steady-state path.
    if let Some(host_addr) = hb.host_addr.as_deref() {
        if !state.host_registry.contains(host_id) {
            tracing::info!(
                host_id = %host_id,
                host_addr = %host_addr,
                "heartbeat for unregistered host; warming pool + registering",
            );
            state
                .services
                .host_pool
                .warm(host_id, host_addr.to_string())
                .await?;
            let client = state.services.host_pool.get(host_id)?;
            let backend: std::sync::Arc<dyn engram_core::traits::HostClient> =
                std::sync::Arc::new(client);
            state.host_registry.register(host_id, backend);
        }
    }

    // ADR 0009 §1-§3: reconcile first, so subsequent state updates
    // reflect the post-flip view. Same dispatch as the WS
    // supervisor loop (`api/hosts.rs:266-284`).
    let flipped = state
        .reconciler
        .reconcile_host(&state, host_id, &hb.running_sandboxes)
        .await;
    if !flipped.is_empty() {
        tracing::info!(
            host_id = %host_id,
            count = flipped.len(),
            "heartbeat reconcile flipped missing-sandbox sessions",
        );
    }

    // ADR 0018 Phase B: placeholder NBD-unhealthy observability. Real
    // evac trigger lands in commit 5; this log just confirms the
    // field is flowing end-to-end so operators can validate the
    // protocol upgrade independent of the trigger ship.
    if !hb.nbd_unhealthy.is_empty() {
        tracing::info!(
            host_id = %host_id,
            count = hb.nbd_unhealthy.len(),
            "heartbeat reports nbd-unhealthy sandboxes (trigger wiring lands in commit 5)",
        );
    }

    // Refresh in-memory scheduler view so the next session-create
    // on this pod sees fresh capacity + readiness.
    state.host_registry.update_state(
        host_id,
        HostState {
            capacity: hb.capacity.clone(),
            local_snapshots: hb.local_snapshots.clone(),
            draining: hb.draining,
            ready_images: hb.ready_images.iter().cloned().collect(),
        },
    );

    // Persist capacity to Postgres alongside `last_heartbeat_at`
    // so `/api/hosts` reads stay consistent across coord replicas.
    // Wire `host_addr` stays NULL here — the register endpoint owns
    // that field, and `upsert_host`'s `COALESCE` prevents this
    // heartbeat-shaped path from clobbering it.
    let row_status = if hb.draining {
        HostStatus::Draining
    } else {
        HostStatus::Ready
    };
    let row_capacity = HostCapacity {
        total_gb: 0,
        used_gb: 0,
        total_mib: hb.capacity.total_mib,
        used_mib: hb.capacity.used_mib,
        running_sandboxes: hb.capacity.running_sandboxes,
    };
    if let Err(e) = state
        .services
        .meta
        .touch_host_heartbeat(host_id, row_status, row_capacity)
        .await
    {
        tracing::debug!(host_id = %host_id, error = %e, "heartbeat persistence failed");
    }

    // ADR 0015 M5: ship the coord's authoritative enabled-images
    // set so the host's prefetch loop drives from heartbeat alone.
    // Best-effort: a PG hiccup degrades to no-images for this tick.
    let enabled_images = match state.services.meta.list_enabled_images().await {
        Ok(rows) => enabled_image_refs_from_rows(rows),
        Err(e) => {
            tracing::debug!(host_id = %host_id, error = %e, "list_enabled_images failed");
            Vec::new()
        }
    };

    Ok(Json(HeartbeatResponse {
        server_time: Utc::now(),
        revoked_sessions: Vec::new(),
        enabled_images,
    }))
}

/// ADR 0015 M5: project enabled-image rows down to the wire
/// representation the host consumes — just `(image_uri, manifest_digest)`.
fn enabled_image_refs_from_rows(
    rows: Vec<engram_core::types::EnabledImage>,
) -> Vec<EnabledImageRef> {
    rows.into_iter()
        .map(|row| EnabledImageRef {
            image_uri: row.image_uri,
            manifest_digest: ManifestDigest(row.manifest_digest),
        })
        .collect()
}

// ---- POST /api/hosts/:id/auth/resolve-registry ----

#[derive(Deserialize)]
pub struct ResolveRegistryAuthRequest {
    /// OCI registry host (e.g. `ghcr.io`).
    pub registry_host: String,
}

#[derive(Serialize)]
pub struct ResolveRegistryAuthResponse {
    /// `None` for anonymous / unknown registries; `Some` for an
    /// entry the coord resolved via its `PgAuthResolver`.
    pub creds: Option<RegistryCreds>,
}

#[derive(Serialize)]
pub struct RegistryCreds {
    pub username: String,
    pub password: String,
}

pub async fn resolve_registry_auth(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
    Json(req): Json<ResolveRegistryAuthRequest>,
) -> Result<Json<ResolveRegistryAuthResponse>, ApiError> {
    tracing::debug!(
        host_id = %host_id,
        host = %req.registry_host,
        "host-agent requested OCI auth resolution (HTTP)"
    );
    let creds = state
        .services
        .auth_resolver
        .resolve(&req.registry_host)
        .await
        .map_err(|e| ApiError::Internal(format!("resolve registry auth: {e}")))?
        .map(|c| RegistryCreds {
            username: c.username,
            password: c.password,
        });
    Ok(Json(ResolveRegistryAuthResponse { creds }))
}

// ---- POST /api/sessions/:session_id/harness-events ----

#[derive(Deserialize)]
pub struct HarnessEventRequest {
    pub sandbox_id: SandboxId,
    pub event: HarnessEvent,
    /// Host's wall-clock at observation time. Forwarded for future
    /// per-event-timestamp work; today the persisted row stamps its
    /// own `created_at`.
    pub at: DateTime<Utc>,
}

pub async fn harness_event_ingest(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<HarnessEventRequest>,
) -> Result<StatusCode, ApiError> {
    // Same call the WS supervisor makes
    // (`api/hosts.rs:357-360`). Existing `state.emit` semantics
    // de-dupe back-to-back duplicate harness-idle events; out-of-
    // order arrival across coord pods is safe because session_event
    // rows carry a monotonic `idx` from Postgres.
    crate::state::emit_harness_event(&state, session_id, req.sandbox_id, req.event, req.at).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- POST /api/hosts/:id/idle-eviction-candidates (ADR 0011 #2) ----

#[derive(Deserialize)]
pub struct IdleEvictionCandidatesRequest {
    /// One entry per candidate. `idle_since` is the host's
    /// last-seen activity timestamp for that sandbox; coord-side
    /// can use it for telemetry and for deciding which pipeline
    /// (snapshot+destroy vs immediate dead) to run.
    pub candidates: Vec<IdleCandidate>,
}

#[derive(Deserialize)]
pub struct IdleCandidate {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    #[serde(default)]
    pub idle_since: Option<DateTime<Utc>>,
}

#[derive(Serialize, Default)]
pub struct IdleEvictionCandidatesResponse {
    pub accepted: usize,
    pub failed: usize,
}

// ---- POST /api/hosts/:id/live-manifest (ADR 0016 Phase B) ----

#[derive(Deserialize)]
pub struct LiveManifestPublishRequest {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub manifest_id: uuid::Uuid,
    pub manifest_version: u64,
}

#[derive(Serialize)]
pub struct LiveManifestPublishResponse {
    pub outcome: LiveManifestPublishOutcome,
}

#[derive(Serialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum LiveManifestPublishOutcome {
    Applied,
    Stale,
}

/// ADR 0016 Phase B: host's FlushScheduler tells us a fresh manifest
/// version landed for `(session_id, sandbox_id)`. We UPDATE
/// `sessions.live_disk_manifest_*` gated on `sandbox_id` match; on
/// `Applied` we also bump `chunk_generation` (the Phase C mid-sweep
/// barrier) in the same TX via `MetadataStore::update_live_disk_manifest`.
///
/// `Stale` (UPDATE matched zero rows) is logged as WARN — the host
/// shouldn't retry; the staleness is structural (sandbox destroyed
/// or rebound). Operators chasing repeated WARNs are likely seeing
/// an issue with sandbox-binding bookkeeping, not Phase B.
pub async fn live_manifest_publish(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
    Json(req): Json<LiveManifestPublishRequest>,
) -> Result<Json<LiveManifestPublishResponse>, ApiError> {
    let manifest_ref = engram_core::types::manifest::ManifestRef {
        manifest_id: req.manifest_id,
        version: req.manifest_version,
    };
    let outcome = state
        .services
        .meta
        .update_live_disk_manifest(req.session_id, req.sandbox_id, manifest_ref)
        .await?;
    match outcome {
        engram_core::traits::UpdateOutcome::Applied => {
            tracing::debug!(
                %host_id,
                session_id = %req.session_id,
                sandbox_id = %req.sandbox_id,
                manifest_id = %req.manifest_id,
                manifest_version = req.manifest_version,
                "live_disk_manifest applied",
            );
            Ok(Json(LiveManifestPublishResponse {
                outcome: LiveManifestPublishOutcome::Applied,
            }))
        }
        engram_core::traits::UpdateOutcome::DroppedStale => {
            tracing::warn!(
                %host_id,
                session_id = %req.session_id,
                sandbox_id = %req.sandbox_id,
                manifest_id = %req.manifest_id,
                manifest_version = req.manifest_version,
                "live_disk_manifest dropped as stale (sandbox_id mismatch)",
            );
            Ok(Json(LiveManifestPublishResponse {
                outcome: LiveManifestPublishOutcome::Stale,
            }))
        }
    }
}

pub async fn idle_eviction_candidates(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
    Json(req): Json<IdleEvictionCandidatesRequest>,
) -> Result<Json<IdleEvictionCandidatesResponse>, ApiError> {
    // Run each candidate through the canonical pipeline. The
    // pipeline is idempotent: if another coord pod or another
    // candidate batch already evicted this sandbox, the registry
    // guard at the top of `evict_idle_session` short-circuits.
    let state_for_loop = Arc::clone(&state);
    let mut accepted = 0usize;
    let mut failed = 0usize;
    for candidate in req.candidates {
        match idle_evictor::evict_idle_session(
            &state_for_loop,
            candidate.session_id,
            candidate.sandbox_id,
        )
        .await
        {
            Ok(()) => {
                accepted += 1;
                tracing::info!(
                    host_id = %host_id,
                    session_id = %candidate.session_id,
                    sandbox_id = %candidate.sandbox_id,
                    idle_since = ?candidate.idle_since,
                    "idle session evicted via host-pushed candidate",
                );
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    host_id = %host_id,
                    session_id = %candidate.session_id,
                    sandbox_id = %candidate.sandbox_id,
                    error = %e,
                    "host-pushed idle eviction failed",
                );
            }
        }
    }
    Ok(Json(IdleEvictionCandidatesResponse { accepted, failed }))
}
