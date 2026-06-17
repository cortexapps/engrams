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
//!     idle-eviction candidates (ADR 0011 follow-up #2; since
//!     ADR 0034 a fast Active→Evicting nomination — the eviction
//!     scanner runs the pipeline, never this handler)
//!
//! Each handler is a thin shim over existing logic — the WS path's
//! supervisor loop and the existing `AuthRequestHandler::handle`
//! body stay authoritative.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostLocalSnapshot, HostMetadata, HostRecord, HostStatus,
};
use engram_core::types::SessionState;
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use engram_harness_proto::HarnessEvent;
use engram_protocol::heartbeat::{
    EnabledImageRef, HostCapacityReport, LocalSnapshotReport, ManifestDigest,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

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
        // Issue #229: a mixed-version fleet is a NORMAL transient state
        // during a non-atomic rolling deploy. The skew is now handled
        // structurally — the scheduler drains this host off (the
        // heartbeat-reported `wire_version` gates `host_is_schedulable`),
        // and the host's gRPC server refuses any racing coord→host RPC
        // with a retryable 503 (`SandboxError::WireSkew`). This stays a
        // warn for operator visibility into how much of the fleet is mid-skew.
        tracing::warn!(
            host_id = %req.host_id,
            host_wire_version = req.wire_version,
            coord_wire_version = engram_protocol::WIRE_VERSION,
            "wire_version mismatch on host register; host will be drained from \
             scheduling until it rolls to the coordinator's version (issue #229)",
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
        // Registration carries no utilization yet — populated on the
        // first heartbeat.
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: Some(req.host_addr.clone()),
        // ADR 0047: scheduling state arrives with the first heartbeat.
        // NOTE: `upsert_host` deliberately does not write `cordoned` —
        // a re-registering host must not clear an operator cordon.
        ready_images: Vec::new(),
        local_snapshots: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        // Issue #229: register carries the host's wire version, but the
        // scheduling-state columns (this among them) are owned by the
        // heartbeat path — `upsert_host` deliberately doesn't write them.
        // The first heartbeat persists the version the scheduler filters
        // on; carry it here so the in-memory record is consistent.
        wire_version: req.wire_version,
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

/// Serde default for `HeartbeatRequest::running_sandboxes_known`
/// (issue #215): a heartbeat that omits the field (pre-fix host-agent
/// mid-roll) is assumed to carry a valid `backend.list()` result.
fn default_running_sandboxes_known() -> bool {
    true
}

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
    /// Issue #215: `false` iff the host's `backend.list()` failed this
    /// tick — `running_sandboxes` is then meaningless and the ADR 0009
    /// reconcile MUST be skipped (an empty list there is "no info", not
    /// "no sandboxes", and would strike every active session on the
    /// host). `#[serde(default = ...)]` to `true` so a pre-fix
    /// host-agent mid-roll (which omits the field) is treated as
    /// carrying a valid list — same behaviour as before the fix.
    #[serde(default = "default_running_sandboxes_known")]
    pub running_sandboxes_known: bool,
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
    /// ADR 0035: the host's bake-stamp bundle set (`drive_id` →
    /// sha256 as refs). `#[serde(default)]` — coord rolls before the
    /// host MIG, so old hosts mid-roll simply report none.
    #[serde(default)]
    pub current_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// ADR 0028 Fix A: un-acked durable checkpoint records from this
    /// host. The handler reconciles each into PG (idempotent on
    /// snapshot_id) and acks the recorded ids — what makes "the
    /// checkpoint reached GCS" become a PG row regardless of which
    /// coord (if any) survived the original capture pipeline.
    #[serde(default)]
    pub checkpoints: Vec<engram_protocol::heartbeat::CheckpointAdvert>,
    /// Observed disk/mem/cpu utilization this tick. Persisted to the
    /// `hosts` row (migration 0056) so `/api/hosts` and placement read
    /// consistently across coord replicas. `#[serde(default)]`
    /// so a pre-utilization host-agent mid-roll reports none → 0.
    #[serde(default)]
    pub utilization: engram_core::types::host::HostUtilization,
    /// ADR 0048: the host's core count, for the CPU packing budget.
    /// `#[serde(default)]` → 0 (= unknown) from pre-roll host-agents.
    #[serde(default)]
    pub total_vcpus: u32,
    /// Issue #229: the host-agent's bincode `engram_protocol::WIRE_VERSION`.
    /// The scheduler excludes a host reporting a nonzero version that
    /// differs from the coordinator's, so a non-atomic rolling deploy
    /// drains off stale hosts instead of hard-failing on them.
    /// `#[serde(default)]` → 0 (= unknown) from a pre-0066 host-agent
    /// mid-roll, which the placement filter tolerates.
    #[serde(default)]
    pub wire_version: u32,
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
    /// ADR 0035 §5: the bundle pin set (every generation some
    /// snapshot row references). Drives the host's bundle
    /// prefetch + sweep supervisor. Always present — the host side
    /// deliberately hard-fails decode when it's missing rather than
    /// treating "absent" as "empty pin set" (which would sweep
    /// generations resumes still need).
    pub live_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// ADR 0028 Fix A: adverts from this heartbeat now recorded in
    /// PG. The host deletes the matching durable record files.
    #[serde(default)]
    pub acked_checkpoints: Vec<engram_core::types::SnapshotId>,
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
        // (Re)warm + register when EITHER the host is unknown to this
        // coord pod (it was rolled by helm-deploy — the original ADR 0013
        // self-heal), OR its advertised addr changed since we last dialed
        // it. The latter is ADR 0044 K2 (GAP 1): a DaemonSet pod restart
        // keeps a STABLE HostId but gets a fresh POD_IP, so `contains` is
        // still true — without the addr-change arm we'd keep dialing the
        // dead old IP forever. `warm` is idempotent (a no-op when the addr
        // is unchanged), and the `register` below re-points the dispatch
        // backend; `update_state` later in this handler repopulates the
        // reset HostState from this same heartbeat.
        let known = state.host_registry.contains(host_id);
        let addr_changed =
            state.services.host_pool.current_addr(host_id).as_deref() != Some(host_addr);
        if !known || addr_changed {
            tracing::info!(
                host_id = %host_id,
                host_addr = %host_addr,
                known,
                addr_changed,
                "heartbeat: (re)warming pool + registering (new host or changed dial addr)",
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
    //
    // Issue #215: skip reconcile when the host couldn't enumerate its
    // sandboxes this tick (`running_sandboxes_known == false`). The
    // empty `running_sandboxes` it sends is "no information", not "no
    // sandboxes running"; reconciling against it would strike every
    // active session on the host on a single host-side `list()` blip.
    if !hb.running_sandboxes_known {
        tracing::warn!(
            host_id = %host_id,
            "heartbeat: host reported running_sandboxes_known=false (backend.list() failed); skipping reconcile this tick",
        );
    } else {
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
    }

    // Bump the routing cache's freshness stamp for this host (ADR
    // 0015 M3). The scheduling payload itself goes to PG below — ADR
    // 0047 removed the in-memory scheduler mirror.
    state.host_registry.touch_seen(host_id);

    // ADR 0047: the single per-heartbeat persist — capacity +
    // utilization + the scheduling state every coordinator replica
    // places from (ready_images / local_snapshots / current_bundles /
    // total_vcpus). `status` is the HOST-reported side (`draining` =
    // the agent's own shutdown flag); the coordinator-owned `cordoned`
    // bit is deliberately not written here.
    //
    // Issue #231: this persist is NOT best-effort. It advances the
    // host's `last_heartbeat_at`, which is exactly what the dead-host
    // detector keys on (`list_stale_hosts`: ready + last_heartbeat_at
    // older than the stale threshold → mark dead + orphan every session
    // on the host). If we swallow the error and ack 200, an asymmetric
    // PG failure — this pod's pool saturated while a sibling pod's
    // detector is healthy — silently staled a *live* host's row and
    // orphaned its sessions, with the host getting 200s the whole time
    // so it neither retried nor re-registered. So on failure we count
    // the metric, warn, and return 5xx: the host's loop tolerates a
    // failed tick and a 5xx engages its backoff/re-register and makes
    // the failure visible in the host's own metrics.
    let row_status = if hb.draining {
        HostStatus::Draining
    } else {
        HostStatus::Ready
    };
    let row_heartbeat = HostHeartbeat {
        status: row_status,
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: hb.capacity.total_mib,
            used_mib: hb.capacity.used_mib,
            running_sandboxes: hb.capacity.running_sandboxes,
        },
        utilization: hb.utilization.clone(),
        ready_images: hb
            .ready_images
            .iter()
            .map(|d| d.as_str().to_string())
            .collect(),
        local_snapshots: hb
            .local_snapshots
            .iter()
            .map(|s| HostLocalSnapshot {
                snapshot_id: s.snapshot_id,
                session_id: s.session_id,
                size_bytes: s.size_bytes,
                replicated: s.replicated,
                last_accessed_at: s.last_accessed_at,
            })
            .collect(),
        current_bundles: hb.current_bundles.clone(),
        total_vcpus: hb.total_vcpus,
        wire_version: hb.wire_version,
    };
    if let Err(e) = state
        .services
        .meta
        .touch_host_heartbeat(host_id, row_heartbeat)
        .await
    {
        ::metrics::counter!(crate::metrics::HEARTBEAT_PERSIST_FAILURES_TOTAL).increment(1);
        tracing::warn!(host_id = %host_id, error = %e, "heartbeat persistence failed; returning 5xx so the host backs off — its last_heartbeat_at did NOT advance and the dead-host detector keys on it (issue #231)");
        return Err(ApiError::Internal(format!(
            "heartbeat persistence failed; ack withheld so the host retries: {e}"
        )));
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

    // ADR 0035 §5: the bundle pin set. NOT best-effort — an empty set
    // is an instruction to sweep, so a PG failure here must fail the
    // heartbeat (the host retries next tick) rather than degrade to
    // "nothing is pinned".
    let live_bundles = state.services.meta.bundle_pin_set().await.map_err(|e| {
        ApiError::Internal(format!(
            "bundle_pin_set failed; heartbeat ack withheld: {e}"
        ))
    })?;

    // ADR 0028 Fix A: reconcile the host's un-acked durable checkpoint
    // records into PG. `record_snapshot` is idempotent on snapshot_id
    // (the eviction pipeline may already have recorded one of these —
    // the re-record is a harmless refresh), so a checkpoint that
    // reached GCS becomes a PG row regardless of which coord (if any)
    // survived the original capture. Per-advert best-effort: an
    // un-acked advert just rides the next heartbeat.
    let mut acked_checkpoints = Vec::new();
    for adv in &hb.checkpoints {
        let events_cursor = state
            .services
            .meta
            .latest_event_idx_at_or_before(adv.session_id, adv.paused_at)
            .await
            .unwrap_or_default();
        let recoverable = crate::api::snapshot::verify_snapshot_recoverable(
            state.services.blob.as_ref(),
            adv.disk_manifest.as_ref(),
            adv.memory_manifest.as_ref(),
        )
        .await;
        let record = engram_core::types::snapshot::SnapshotRecord {
            id: adv.snapshot_id,
            session_id: Some(adv.session_id),
            host_id: Some(host_id),
            image_version: adv.image_version.clone(),
            size_bytes: adv.size_bytes,
            created_at: adv.captured_at,
            last_accessed_at: Utc::now(),
            disk_manifest: adv.disk_manifest,
            memory_manifest: adv.memory_manifest,
            recoverable,
            aux_bundles: adv.aux_bundles.clone(),
            events_cursor,
        };
        match state.services.meta.record_snapshot(record).await {
            Ok(()) => acked_checkpoints.push(adv.snapshot_id),
            Err(e) => {
                tracing::warn!(
                    host_id = %host_id,
                    snapshot_id = %adv.snapshot_id,
                    session_id = %adv.session_id,
                    error = %e,
                    "checkpoint advert reconcile failed; host re-advertises next heartbeat",
                );
            }
        }
    }
    if !acked_checkpoints.is_empty() {
        tracing::info!(
            host_id = %host_id,
            count = acked_checkpoints.len(),
            "reconciled host-advertised checkpoints into PG",
        );
    }

    Ok(Json(HeartbeatResponse {
        server_time: Utc::now(),
        revoked_sessions: Vec::new(),
        enabled_images,
        live_bundles,
        acked_checkpoints,
    }))
}

/// ADR 0015 M5: project enabled-image rows down to the wire
/// representation the host consumes — `(image_uri, manifest_digest)` plus,
/// since ADR 0021 P2, the base snapshot's disk manifest so the host can warm
/// the rootfs working set on NVMe (residency) before sessions restore.
fn enabled_image_refs_from_rows(
    rows: Vec<engram_core::types::EnabledImage>,
) -> Vec<EnabledImageRef> {
    rows.into_iter()
        .filter_map(|row| {
            // base_snapshot_disk_manifest is NOT NULL (migration 0042), so a
            // persisted row always has it — the Option is only the build-then-
            // stamp shape. Defensively skip (rather than panic) the impossible
            // None so one malformed row can't break the whole advertisement.
            let Some(base_snapshot_disk_manifest) = row.base_snapshot_disk_manifest else {
                tracing::error!(
                    image_uri = %row.image_uri,
                    "enabled image has no base_snapshot_disk_manifest (NOT NULL invariant violated); not advertising",
                );
                return None;
            };
            // ADR 0022 Option A: base_snapshot_id is NOT NULL (migration 0038);
            // same defensive skip — the host needs it to key the per-template
            // memfile residency path.
            let Some(base_snapshot_id) = row.base_snapshot_id else {
                tracing::error!(
                    image_uri = %row.image_uri,
                    "enabled image has no base_snapshot_id (NOT NULL invariant violated); not advertising",
                );
                return None;
            };
            // ADR 0021 P2 (memory residency): nullable since migration 0049.
            // `None` for cold-boot backends (VZ) that capture a disk-only base
            // snapshot — advertise the row anyway; the host's prefetch warms
            // only the disk tier when memory is absent.
            Some(EnabledImageRef {
                image_uri: row.image_uri,
                manifest_digest: ManifestDigest(row.manifest_digest),
                base_snapshot_id,
                base_snapshot_disk_manifest,
                base_snapshot_memory_manifest: row.base_snapshot_memory_manifest,
            })
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

/// ADR 0034: fast control-plane nomination. The pre-0034 handler ran
/// the full snapshot pipeline inline here; a fat session's eviction
/// (60-90s+) outlived the host's POST timeout, the connection drop
/// cancelled the handler future mid-pipeline, and the eviction died
/// silently (prod session 0782bea5 sat Active for 8+ hours). Now the
/// handler only flips `Active → Evicting` — a single PG UPDATE — and
/// the eviction scanner (`idle_evictor::spawn_eviction_scanner`)
/// drives the pipeline outside any request lifetime.
///
/// Response semantics: `accepted` = "the candidate is in (or already
/// past) the Evicting lane" — including the no-op cases below;
/// `failed` = "couldn't read or transition the row." The host
/// re-nominates a still-Evicting sandbox on every 10s tick until the
/// pipeline destroys it; counting those re-nominations as accepted
/// no-ops is what keeps that loop silent and cheap.
/// ADR 0045 C1: the migration export TTL's dumb-host ownership check.
/// A source host-agent holding a frozen export past its TTL (no
/// commit/abort arrived — a coordinator death mid-move) asks: "does
/// session X still bind my sandbox Y?" `true` ⇒ the move never landed,
/// un-pause in place (abort). `false` ⇒ the session moved on (the
/// scanner rehomed it, or the rebind landed without the commit) —
/// destroying the stale frozen source is safe and REQUIRED (resuming
/// it would split state).
pub async fn sandbox_ownership(
    State(state): State<SharedState>,
    Path((_host_id, session_id, sandbox_id)): Path<(HostId, SessionId, SandboxId)>,
) -> Result<Json<SandboxOwnershipResponse>, ApiError> {
    let owned = match state.services.meta.get_session(session_id).await {
        Ok(s) => s.sandbox_id == Some(sandbox_id),
        Err(engram_core::MetaError::NotFound) => false,
        Err(e) => return Err(ApiError::Internal(format!("get_session: {e}"))),
    };
    Ok(Json(SandboxOwnershipResponse { owned }))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SandboxOwnershipResponse {
    pub owned: bool,
}

pub async fn idle_eviction_candidates(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
    Json(req): Json<IdleEvictionCandidatesRequest>,
) -> Result<Json<IdleEvictionCandidatesResponse>, ApiError> {
    let mut accepted = 0usize;
    let mut failed = 0usize;
    for candidate in req.candidates {
        let session = match state.services.meta.get_session(candidate.session_id).await {
            Ok(s) => s,
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    host_id = %host_id,
                    session_id = %candidate.session_id,
                    sandbox_id = %candidate.sandbox_id,
                    error = %e,
                    "idle-eviction candidate: session lookup failed",
                );
                continue;
            }
        };
        if session.status != SessionState::Active {
            // Already Evicting (the common re-nomination case) or
            // moved on entirely (deleted, host-lost). Either way the
            // work is queued or moot — accepted no-op.
            accepted += 1;
            continue;
        }
        match state
            .services
            .meta
            .transition_session(candidate.session_id, SessionState::Evicting)
            .await
        {
            Ok(prev) => {
                accepted += 1;
                ::metrics::counter!(crate::metrics::EVICTION_NOMINATED_TOTAL, "source" => "host")
                    .increment(1);
                tracing::info!(
                    host_id = %host_id,
                    session_id = %candidate.session_id,
                    sandbox_id = %candidate.sandbox_id,
                    idle_since = ?candidate.idle_since,
                    "idle session nominated for eviction (scanner drives the pipeline)",
                );
                if let Err(e) = state
                    .emit(
                        candidate.session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Evicting,
                            at: Utc::now(),
                        },
                    )
                    .await
                {
                    // Status is committed; a lost event is timeline
                    // cosmetics, not lifecycle state. Don't fail the
                    // candidate over it.
                    tracing::warn!(
                        session_id = %candidate.session_id,
                        error = %e,
                        "idle-eviction nomination: StatusChanged emit failed",
                    );
                }
            }
            // Lost the Active-check race (another pod's nomination,
            // a concurrent delete): the row is wherever the winner
            // put it — accepted no-op, same as the pre-check path.
            Err(MetaError::Conflict(_)) => accepted += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    host_id = %host_id,
                    session_id = %candidate.session_id,
                    sandbox_id = %candidate.sandbox_id,
                    error = %e,
                    "idle-eviction candidate: transition to Evicting failed",
                );
            }
        }
    }
    Ok(Json(IdleEvictionCandidatesResponse { accepted, failed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::SessionMode;
    use engram_core::types::Session;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_for_session(session: Session) -> (SharedState, TempDir) {
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(local.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::with_noop_hub(backend.clone()),
        );
        host_registry.register(engram_core::HostId::new(), local_host);
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, local)
    }

    fn session_with_status(
        id: engram_core::SessionId,
        sandbox: SandboxId,
        status: SessionState,
    ) -> Session {
        Session {
            id,
            status,
            host_id: None,
            sandbox_id: Some(sandbox),
            image: "test/repo:evict".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    fn candidates_req(
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> IdleEvictionCandidatesRequest {
        IdleEvictionCandidatesRequest {
            candidates: vec![IdleCandidate {
                session_id,
                sandbox_id,
                idle_since: None,
            }],
        }
    }

    /// Issue #215: a heartbeat that OMITS `running_sandboxes_known`
    /// (pre-fix host-agent mid-roll) must deserialize to `true` so the
    /// coord keeps reconciling against its list — same behaviour as
    /// before the fix. A heartbeat that explicitly sends `false`
    /// (post-fix host that hit a `backend.list()` error) parses as
    /// `false` so the handler skips reconcile for that tick.
    #[test]
    fn running_sandboxes_known_defaults_true_but_honors_false() {
        // Omitted → true (interop with pre-fix host-agents).
        let omitted: HeartbeatRequest = serde_json::from_value(serde_json::json!({
            "capacity": { "total_mib": 1024, "used_mib": 0, "running_sandboxes": 0 },
            "running_sandboxes": [],
        }))
        .expect("deserialize heartbeat without the flag");
        assert!(
            omitted.running_sandboxes_known,
            "a heartbeat omitting the flag must be treated as carrying a valid list"
        );

        // Explicit false → false (host's list() errored this tick).
        let errored: HeartbeatRequest = serde_json::from_value(serde_json::json!({
            "capacity": { "total_mib": 1024, "used_mib": 0, "running_sandboxes": 0 },
            "running_sandboxes": [],
            "running_sandboxes_known": false,
        }))
        .expect("deserialize heartbeat with the flag");
        assert!(
            !errored.running_sandboxes_known,
            "an explicit false must be honored so the coord skips reconcile this tick"
        );
    }

    /// ADR 0034 happy path: the handler flips Active → Evicting,
    /// emits StatusChanged, and returns — it does NOT run the
    /// pipeline (status is Evicting, not Idle; the sandbox binding
    /// is untouched for the scanner to use).
    #[tokio::test]
    async fn handler_nominates_active_session_and_returns() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = SandboxId::new();
        let (state, _local) = build_state_for_session(session_with_status(
            session_id,
            sandbox_id,
            SessionState::Active,
        ));

        let resp = idle_eviction_candidates(
            State(state.clone()),
            Path(HostId::new()),
            Json(candidates_req(session_id, sandbox_id)),
        )
        .await
        .expect("handler");
        assert_eq!(resp.0.accepted, 1);
        assert_eq!(resp.0.failed, 0);

        let session = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(session.status, SessionState::Evicting);
        assert_eq!(
            session.sandbox_id,
            Some(sandbox_id),
            "nomination must not unbind the sandbox — the scanner's pipeline needs it"
        );

        // StatusChanged(Active → Evicting) landed on the event log.
        let events = state
            .services
            .meta
            .list_session_events_since(session_id, -1, -1)
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == "status_changed"
                    && e.payload["to"] == serde_json::json!("evicting")),
            "expected a status_changed→evicting event, got {events:?}"
        );
    }

    /// Re-nomination of an already-Evicting sandbox (the host's 10s
    /// tick while the pipeline runs) is an accepted no-op: no second
    /// transition, no second event.
    #[tokio::test]
    async fn handler_renomination_of_evicting_is_accepted_noop() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = SandboxId::new();
        let (state, _local) = build_state_for_session(session_with_status(
            session_id,
            sandbox_id,
            SessionState::Evicting,
        ));

        let resp = idle_eviction_candidates(
            State(state.clone()),
            Path(HostId::new()),
            Json(candidates_req(session_id, sandbox_id)),
        )
        .await
        .expect("handler");
        assert_eq!(resp.0.accepted, 1, "re-nomination is accepted");
        assert_eq!(resp.0.failed, 0);

        let session = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(session.status, SessionState::Evicting, "unchanged");
        let events = state
            .services
            .meta
            .list_session_events_since(session_id, -1, -1)
            .await
            .unwrap();
        assert!(events.is_empty(), "no event for a no-op, got {events:?}");
    }

    /// A candidate whose session moved on entirely (terminal) is an
    /// accepted no-op — the host must not see it as a failure and
    /// retry-storm it.
    #[tokio::test]
    async fn handler_nonactive_candidate_is_accepted_noop() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = SandboxId::new();
        let (state, _local) = build_state_for_session(session_with_status(
            session_id,
            sandbox_id,
            SessionState::Completed,
        ));

        let resp = idle_eviction_candidates(
            State(state.clone()),
            Path(HostId::new()),
            Json(candidates_req(session_id, sandbox_id)),
        )
        .await
        .expect("handler");
        assert_eq!(resp.0.accepted, 1);
        assert_eq!(resp.0.failed, 0);
        let session = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(session.status, SessionState::Completed, "untouched");
    }

    /// An unknown session is the one genuine failure shape.
    #[tokio::test]
    async fn handler_unknown_session_counts_failed() {
        let session_id = engram_core::SessionId::new();
        let sandbox_id = SandboxId::new();
        let (state, _local) = build_state_for_session(session_with_status(
            engram_core::SessionId::new(), // different id than nominated
            sandbox_id,
            SessionState::Active,
        ));

        let resp = idle_eviction_candidates(
            State(state),
            Path(HostId::new()),
            Json(candidates_req(session_id, sandbox_id)),
        )
        .await
        .expect("handler");
        assert_eq!(resp.0.accepted, 0);
        assert_eq!(resp.0.failed, 1);
    }
}
