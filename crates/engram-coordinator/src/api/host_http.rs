//! ADR 0013 host → coord HTTP endpoints.
//!
//! Replaces the host-initiated bits of the WS protocol with plain
//! HTTP/JSON POSTs that any coord pod can serve. Wired into the
//! host-agent's real heartbeat loop / event sink / eviction-pusher
//! (`engram-host-agent::coord_client`).
//!
//! Endpoints:
//!   - `POST /api/hosts/register` — once per host-agent startup
//!   - `POST /api/hosts/:id/heartbeat` — every 5s
//!   - `POST /api/hosts/:id/auth/resolve-registry` — host requests
//!     OCI creds during image pull
//!   - `POST /api/hosts/:id/capture-jobs/:job_id/claim` (ADR 0084 P1b) —
//!     host resolves the full dispatch for a `HeartbeatAck.
//!     capture_assignments` entry it doesn't yet own
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
use engram_core::types::host::{HostCapacity, HostHeartbeat, HostMetadata, HostRecord, HostStatus};
use engram_core::types::BindingDisposition;
use engram_core::{HostId, SandboxId, SessionId, SessionState};
use engram_harness_proto::HarnessEvent;
use engram_protocol::heartbeat::{EnabledImageRef, HostCapacityReport, ManifestDigest};
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
    /// ADR 0068: the host's self-verified capability vector, probed
    /// before this register POST. `#[serde(default)]` for interop with
    /// a pre-0068 host-agent (decodes to `schema: 0`, soft-tolerated by
    /// `host_meets_capabilities`).
    #[serde(default)]
    pub capabilities: engram_core::types::host::HostCapabilities,
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
/// `MetadataStore::list_resident_sandboxes_on_host_with_disk_manifest`
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
        last_heartbeat_at: state.services.clock.now_utc(),
        host_addr: Some(req.host_addr.clone()),
        // ADR 0047: scheduling state arrives with the first heartbeat.
        // NOTE: `upsert_host` deliberately does not write `cordoned` —
        // a re-registering host must not clear an operator cordon.
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        // Issue #229: register carries the host's wire version, but the
        // scheduling-state columns (this among them) are owned by the
        // heartbeat path — `upsert_host` deliberately doesn't write them.
        // The first heartbeat persists the version the scheduler filters
        // on; carry it here so the in-memory record is consistent.
        wire_version: req.wire_version,
        // ADR 0036 amendment (issue #538): scheduling state, same as
        // `wire_version` above — the first heartbeat persists the real
        // value; `upsert_host` doesn't write this column at all.
        stages_images: false,
        // ADR 0068: persist the register-time vector too — see the
        // `upsert_host` doc comment on why a first-row host shouldn't
        // sit at `schema: 0` until its first heartbeat.
        capabilities: req.capabilities,
        // ADR 0116 A-D3: a register is a host-agent generation adopting
        // the host — the store REPLACES the lease deadline with this
        // (ending any handoff early), flips it Active, and bumps
        // `lease_epoch`. Coordinator-side regardless of host version.
        lease_expires_at: Some(state.services.clock.now_utc() + crate::config::host_lease_ttl()),
        lease_state: Default::default(),
        lease_epoch: 0,
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
    // knows which VM-resident sessions are bound to this host (a
    // host-agent restart drops `nbd_sandboxes` but PG persists the
    // session→sandbox→host binding). "Resident" = every
    // `reserves_host_memory` state with a sandbox bound, NOT just
    // Active: a rung-parked Evicting session's paused VM survives
    // the pod roll too, and omitting it orphans its NBD device
    // (session 731df805, 2026-07-17). Hand the list back so the
    // host can rebuild `ChunkedDiskBackend` + spawn the
    // FlushScheduler for each, restoring continuous-flush
    // coverage. Failure non-fatal: a host that doesn't get the
    // list runs blind for the survivors (same shape as pre-Phase-
    // B behaviour); operator can manually re-register or evac.
    let rehydrate_sandboxes = match register_rehydrate_list_core(&state, req.host_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                host_id = %req.host_id,
                error = %e,
                "rehydrate-list query failed at register; host will run blind for survivors",
            );
            Vec::new()
        }
    };

    // ADR 0111: survivor egress recovery is host-local. The host
    // persists every applied policy beside its binding record and
    // rebuilds its proxy registry from those files at startup — the
    // coordinator no longer re-pushes policies after a host restart
    // (the 2026-07-13 dfa0face re-push watchdog is retired).

    tracing::info!(
        host_id = %req.host_id,
        host_addr = %req.host_addr,
        agent_version = %req.agent_version,
        enabled_images = enabled_images.len(),
        rehydrate_sandboxes = rehydrate_sandboxes.len(),
        "host registered via /api/hosts/register",
    );

    Ok(Json(RegisterResponse {
        server_time: state.services.clock.now_utc(),
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
    /// ADR 0035 amendment D2: aux bundle generations attached to each running
    /// sandbox this tick. Persisted on the hosts row and unioned into
    /// `bundle_pin_set` so a live-but-unsnapshotted sandbox pins its
    /// generations. `#[serde(default)]` — an old host mid-roll reports
    /// none; the bundle GC's grace period covers that window.
    #[serde(default)]
    pub sandbox_bundles: Vec<engram_core::types::sandbox::SandboxAuxBundles>,
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
    /// ADR 0036 amendment (issue #538): true iff this host's image-prefetch
    /// supervisor is spawned (`chunk_store` + `chunk_cache` configured). The
    /// enable scanner's prestage stage waits only on hosts reporting this —
    /// a fleet with zero eligible staging hosts passes the stage vacuously.
    /// `#[serde(default)]` → `false` from a pre-0538 host-agent mid-roll,
    /// the safe/exempt posture.
    #[serde(default)]
    pub stages_images: bool,
    /// ADR 0068: this tick's re-probed capability vector.
    /// `#[serde(default)]` → `schema: 0` (soft-tolerated) from a
    /// pre-0068 host-agent mid-roll.
    #[serde(default)]
    pub capabilities: engram_core::types::host::HostCapabilities,
    /// ADR 0073 phase 4: hub-attached sandboxes (see the host-side
    /// twin). Disagreement-alarm input only.
    #[serde(default)]
    pub harness_attached: Vec<SandboxId>,
    /// ADR 0084 P1b: un-acked `capture_jobs` progress/terminal reports
    /// from this host's durable capture-job records — the
    /// `CheckpointAdvert`/`checkpoints` pattern verbatim, for capture.
    #[serde(default)]
    pub capture_job_reports: Vec<engram_core::types::CaptureJobReport>,
    /// ADR 0090: survivors whose NBD slot the host quarantined after a
    /// failed rehydrate — the coordinator enqueues `evict_local` for
    /// each still-owned one (the op layer dedups re-adverts).
    #[serde(default)]
    pub quarantined_survivors: Vec<engram_protocol::heartbeat::QuarantinedSurvivor>,
    /// ADR 0091: control-plane-dead guests (host's 3/3-probe verdict) —
    /// the handler flips each owning session Active → Unreachable.
    #[serde(default)]
    pub unreachable_guests: Vec<(SandboxId, SessionId)>,
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
    /// ADR 0036 amendment (issue #538): base-snapshot refs of images
    /// currently in the `prestaging` enable-job stage — advertised so
    /// eligible hosts warm them via the SAME prefetch supervisor path as
    /// `enabled_images` (the supervisor consumes the deduped union), before
    /// the enable scanner's `enabled_images` upsert makes the digest
    /// visible to session-create. `#[serde(default)]` for interop: a
    /// pre-0538 host-agent ignores the field.
    #[serde(default)]
    pub prestage_images: Vec<EnabledImageRef>,
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
    /// ADR 0084 P1b: `(job_id, epoch)` assignments this host should be
    /// running (or should claim, if it isn't yet) — read unconditionally
    /// every tick via `capture_assignments_for_host`. `None` means the
    /// read FAILED (unknown, do nothing); `Some(vec![])` means the
    /// authoritative answer is "no assignments" — the host cancels any
    /// still-running attempt absent from a `Some` list (it was
    /// reassigned away or terminally superseded). Collapsing an error
    /// into an empty list would make a PG blip destroy healthy in-flight
    /// captures fleet-wide.
    #[serde(default)]
    pub capture_assignments: Option<Vec<engram_core::types::CaptureJobAssignment>>,
    /// ADR 0084 P1b: terminal reports from this heartbeat that landed in
    /// PG (or were already terminal at a matching epoch) — the host
    /// deletes the matching durable capture-job records.
    #[serde(default)]
    pub acked_capture_jobs: Vec<engram_core::types::CaptureJobId>,
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

    // ADR 0068: persist BEFORE reconcile (was the reverse — see the
    // git history on this handler). A heartbeat the coordinator
    // refuses to ack (5xx on persist failure, issue #231) must not
    // have ALREADY driven session flips from its `running_sandboxes`
    // payload — "Postgres is the authority" means reconcile can only
    // act on a heartbeat that's actually landed. This closes one of
    // the two candidate producers of the fbd3794c incident shape (the
    // other, the dead-host detector's own probe, already gates
    // correctly — see `dead_host.rs`).
    //
    // ADR 0047: the single per-heartbeat persist — capacity +
    // utilization + the scheduling state every coordinator replica
    // places from (ready_images / current_bundles /
    // total_vcpus / capabilities). `status` is the HOST-reported side
    // (`draining` = the agent's own shutdown flag); the
    // coordinator-owned `cordoned` bit is deliberately not written here.
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
        current_bundles: hb.current_bundles.clone(),
        sandbox_bundles: hb.sandbox_bundles.clone(),
        total_vcpus: hb.total_vcpus,
        wire_version: hb.wire_version,
        stages_images: hb.stages_images,
        capabilities: hb.capabilities.clone(),
        // ADR 0116 A-D3: the coordinator renews the binding lease on
        // EVERY heartbeat, regardless of host-agent version — the renewal
        // rides the same single UPDATE as the persist (GREATEST inside,
        // so a racing predecessor can never shrink a handoff deadline).
        lease_renew_until: Some(state.services.clock.now_utc() + crate::config::host_lease_ttl()),
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

    // ADR 0009 §1-§3: reconcile AFTER the persist above lands — a
    // heartbeat we 5xx'd never reaches here (early return above).
    // Same dispatch as the WS supervisor loop (`api/hosts.rs:266-284`).
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
    // 0015 M3). The scheduling payload itself lives in PG above — ADR
    // 0047 removed the in-memory scheduler mirror.
    state.host_registry.touch_seen(host_id);
    // ADR 0073 phase 4 → ADR 0108 A8: the disagreement check is a
    // REPAIR now, not just an alarm. An Active session whose sandbox
    // the host reports RUNNING but not hub-ATTACHED has a delivery
    // hole: a forward into that hub reaches a socket with no reader,
    // and the outbox row waits out the full ACK_TIMEOUT (prod
    // 7eddce62). `harness_desync::run_once` recalls the waiting rows
    // and enqueues the Deliver op directly; the decision logic lives
    // there — not here — so the DST swarm drives it without this HTTP
    // handler. Best-effort and deliberately coarse: sessions
    // mid-create/resume legitimately have no attach yet, and the
    // repair only recalls rows that were already waiting.
    if hb.running_sandboxes_known && !hb.running_sandboxes.is_empty() {
        let running: std::collections::BTreeSet<_> = hb.running_sandboxes.iter().copied().collect();
        let attached: std::collections::BTreeSet<_> = hb.harness_attached.iter().copied().collect();
        let _ = crate::harness_desync::run_once(&state, host_id, &running, &attached).await;
    }

    // ADR 0015 M5: ship the coord's authoritative enabled-images
    // set so the host's prefetch loop drives from heartbeat alone.
    // Best-effort: a PG hiccup degrades to no-images for this tick.
    let mut enabled_images = match state.services.meta.list_enabled_images().await {
        Ok(rows) => enabled_image_refs_from_rows(rows),
        Err(e) => {
            tracing::debug!(host_id = %host_id, error = %e, "list_enabled_images failed");
            Vec::new()
        }
    };

    // ADR 0036 amendment (issue #538): ship the base-snapshot refs of
    // every enable job currently `prestaging`, so eligible hosts warm them
    // BEFORE the scanner's `enabled_images` upsert makes the digest
    // visible to session-create. Best-effort, same posture as
    // `enabled_images` above — a PG hiccup degrades to no prestage work
    // for this tick; the scanner's poll loop just sees one more empty
    // heartbeat and keeps waiting. A row that fails to deserialize (wire
    // skew mid-roll) is skipped + logged rather than failing the whole ack.
    let mut prestage_images: Vec<EnabledImageRef> =
        match state.services.meta.list_prestaging_refs().await {
            Ok(raw) => raw
                .into_iter()
                .filter_map(|v| match serde_json::from_value::<EnabledImageRef>(v) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(host_id = %host_id, error = %e, "prestage_ref failed to deserialize; skipping");
                        None
                    }
                })
                .collect(),
            Err(e) => {
                tracing::debug!(host_id = %host_id, error = %e, "list_prestaging_refs failed");
                Vec::new()
            }
        };

    // ADR 0095: stamp peer-fill seeds onto every image entry — fleet
    // siblings whose `ready_images` already carry the digest, so the
    // recipient's prefetch supervisor pulls the base chunk set over the
    // LAN instead of N-hosts × GCS. Freshly assembled every ack (never
    // persisted; seeds change as hosts warm/die). Best-effort, same
    // posture as the lists themselves: a PG hiccup ⇒ no seeds ⇒ pure
    // GCS, byte-identical to pre-0095. The capturing host of a fresh
    // enable flips ready within one reconcile tick of the snapshot row
    // landing (its prefetch is an all-local stat walk), so it becomes
    // the prestage seed automatically, and hosts that finish warming
    // join the seed set — a natural fan-out tree.
    if !(enabled_images.is_empty() && prestage_images.is_empty()) {
        match state.services.meta.list_active_hosts().await {
            Ok(hosts) => {
                let now = state.services.clock.now_utc();
                attach_warm_peers(&mut enabled_images, &hosts, host_id, now);
                attach_warm_peers(&mut prestage_images, &hosts, host_id, now);
            }
            Err(e) => {
                tracing::debug!(host_id = %host_id, error = %e, "list_active_hosts failed; no warm peers this tick");
            }
        }
    }

    // ADR 0035 §5: the bundle pin set. NOT best-effort — an empty set
    // is an instruction to sweep, so a PG failure here must fail the
    // heartbeat (the host retries next tick) rather than degrade to
    // "nothing is pinned".
    //
    // ADR 0035 amendment D2 ordering invariant: this read runs strictly AFTER
    // `touch_host_heartbeat` persisted THIS tick's `sandbox_bundles`
    // (early-return on failure above), so a freshly rolled host's
    // FIRST ack already pins its own reattached sandboxes'
    // attachments — the ack drives the host's sweep, and this exact
    // window (roll → reattach → first sweep before any pin existed)
    // is what destroyed a running VM's only bundle copy on
    // 2026-08-10. Do not move this read above the persist.
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
            last_accessed_at: state.services.clock.now_utc(),
            disk_manifest: adv.disk_manifest,
            memory_manifest: adv.memory_manifest,
            recoverable,
            aux_bundles: adv.aux_bundles.clone(),
            events_cursor,
            // ADR 0068: this heartbeat's own capability vector already
            // carries the recording host's FC snapshot-version — no
            // separate lookup needed (unlike the eviction pipeline,
            // which stamps from a different host's perspective and has
            // to ask PG for it).
            fc_snapshot_version: hb.capabilities.fc_snapshot_version.clone(),
        };
        match state.services.meta.record_snapshot(record).await {
            Ok(inserted) => {
                acked_checkpoints.push(adv.snapshot_id);
                // Issue #529: the eviction finalize job is host-owned and
                // never talks to the coordinator again once
                // `snapshot_begin` returns — THIS reconcile, on the row's
                // first landing, is now the only place `SnapshotTaken`
                // emits for a D5 eviction. A periodic checkpoint's row
                // (or a re-record of either kind) must NOT re-emit it.
                if inserted && adv.kind == engram_protocol::heartbeat::CheckpointKind::EvictionFinal
                {
                    let _ = state
                        .emit(
                            adv.session_id,
                            SessionEvent::SnapshotTaken {
                                snapshot_id: adv.snapshot_id,
                                size_bytes: adv.size_bytes,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                }
                // ADR 0101 C: the durability-floor settle. The evict op
                // no longer flips Idle at capture time — the session is
                // honestly `evicting` until THIS reconcile records the
                // recoverable row, then one guarded UPDATE detaches the
                // sandbox and flips `Evicting → Idle`. Idempotent + CAS:
                // a re-record, a session that already settled, a rebound
                // successor sandbox, or a non-recoverable row all make it
                // a clean `false` no-op. Runs on EVERY eviction-final
                // advert (not just first landing): if the settle itself
                // raced/failed once, the host's re-advert retries it.
                if recoverable
                    && adv.kind == engram_protocol::heartbeat::CheckpointKind::EvictionFinal
                {
                    // The settle's lifecycle facts ride ITS transaction:
                    // the settle is CAS-once (a re-advert after success is
                    // a clean no-op), so facts emitted here afterwards had
                    // a crash window in which they were lost forever —
                    // the same event-loss class as the fenced
                    // post-transition emits, crash-shaped. See the trait
                    // doc on `settle_evicted_session_idle`.
                    let now = state.services.clock.now_utc();
                    let events = vec![
                        SessionEvent::Evicted { at: now },
                        SessionEvent::StatusChanged {
                            from: SessionState::Evicting,
                            to: SessionState::Idle,
                            at: now,
                        },
                    ];
                    let wire = match crate::state::wire_events(&events) {
                        Ok(wire) => wire,
                        Err(e) => {
                            tracing::warn!(
                                session_id = %adv.session_id,
                                error = %e,
                                "eviction settle: event serialize failed; \
                                 the host's re-advert retries it",
                            );
                            continue;
                        }
                    };
                    match state
                        .services
                        .meta
                        .settle_evicted_session_idle(
                            adv.session_id,
                            adv.sandbox_id,
                            adv.snapshot_id,
                            &wire,
                        )
                        .await
                    {
                        Ok(Some(indices)) => {
                            // Best-effort rung clear (the park stamp is
                            // ascent/ledger metadata; lifecycle already
                            // settled above).
                            let _ = state
                                .services
                                .meta
                                .set_session_park_rung(adv.session_id, 0, None)
                                .await;
                            for (idx, event) in indices.into_iter().zip(events) {
                                state.events.publish(
                                    adv.session_id,
                                    crate::state::IndexedEvent {
                                        idx,
                                        event,
                                        ephemeral: false,
                                    },
                                );
                            }
                            tracing::info!(
                                session_id = %adv.session_id,
                                snapshot_id = %adv.snapshot_id,
                                "eviction settled Idle: recoverable snapshot row landed \
                                 (ADR 0101 C durability floor)",
                            );
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(
                                session_id = %adv.session_id,
                                snapshot_id = %adv.snapshot_id,
                                error = %e,
                                "eviction settle failed; the host's re-advert retries it",
                            );
                        }
                    }
                }
            }
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

    // ADR 0090: drive the documented remediation for quarantined
    // survivors (NBD rehydrate failed after a roll — VM possibly live,
    // disk unserved). Enqueue `evict_local` (full capture, no park) for
    // each survivor the session still owns. Dedup keys ONLY on the
    // idempotency key (ADR 0079 finding #4 — active-state-scoped), so the
    // re-adverts every 5s heartbeat carries MUST pass one; a key-less
    // enqueue inserts a fresh queued row per heartbeat (ADR 0093: 423
    // rows piled up behind one wedged evict in the 2026-07-13 incident).
    // Pre-ADR-0090, nothing consumed the host's WARN and the teardown
    // reconciler's orphan path SIGKILLed the VM.
    //
    // 2026-07-21 livelock incident: the active-state-scoped key is only
    // safe because the enqueued op is guaranteed to CONVERGE. An op that
    // settles Done as a fast no-op frees the key before the next
    // heartbeat and turns this enqueue into a 5s-cadence infinite loop
    // (session 8174b7aa: an ADR 0077 harness-failed park at `Created`
    // skipped the evict guard in ~10ms, for 2.5 days / ~43k ops). The
    // evict pipeline's quarantine arm (`quarantine_reap_unevictable`)
    // now destroys the survivor — clearing the host's quarantine entry,
    // i.e. this very advertise — whenever the session can't be evicted
    // from its current state, so every enqueue here ends the loop it
    // rides on. Keep that pairing in mind before adding states the
    // pipeline may skip.
    quarantined_survivor_advertise_core(&state, host_id, &hb.quarantined_survivors).await;

    // ADR 0091: flip sessions whose guest control plane is dead. The
    // host re-advertises until a successful capture or destroy clears
    // the entry, so this is idempotent (already-Unreachable sessions
    // skip). Pre-fix a dead guest read `active` indefinitely with every
    // exec bouncing (2026-07-11 campaign C1: a 16+ minute zombie only
    // in-guest execs could unmask).
    for (sandbox_id, session_id) in &hb.unreachable_guests {
        match state.services.meta.get_session(*session_id).await {
            Ok(s)
                if s.status == engram_core::types::SessionState::Active
                    && s.sandbox_id == Some(*sandbox_id) =>
            {
                match state
                    .services
                    .meta
                    .transition_session(
                        *session_id,
                        engram_core::types::SessionState::Unreachable,
                        BindingDisposition::Retain,
                    )
                    .await
                {
                    Ok(prev) => {
                        tracing::warn!(
                            host_id = %host_id,
                            session_id = %session_id,
                            %sandbox_id,
                            "guest control plane dead — session flipped Unreachable (ADR 0091)",
                        );
                        let _ = state
                            .emit(
                                *session_id,
                                crate::state::SessionEvent::StatusChanged {
                                    from: prev,
                                    to: engram_core::types::SessionState::Unreachable,
                                    at: state.services.clock.now_utc(),
                                },
                            )
                            .await;
                    }
                    Err(e) => tracing::warn!(
                        session_id = %session_id, error = %e,
                        "unreachable flip failed (state raced?); host re-advertises next tick",
                    ),
                }
            }
            // Not Active / rebound elsewhere / unknown — nothing to flip;
            // the host clears its entry on destroy.
            _ => {}
        }
    }

    // ADR 0084 P1b: reconcile this host's un-acked capture-job reports.
    // Each write is fenced by `(job_id, epoch)` — any coord replica can
    // perform it, no lease-holder identity involved. A terminal report is
    // acked when either the fenced write actually landed it, OR the row
    // is already terminal at a MATCHING epoch (idempotent re-advertise —
    // e.g. a prior ack this host somehow didn't observe). A terminal
    // report whose epoch no longer matches (the job was reassigned away)
    // is silently dropped: the host's own stale-epoch bookkeeping evicts
    // that attempt, and a fresh report at the new epoch supersedes it.
    let mut acked_capture_jobs = Vec::new();
    for report in &hb.capture_job_reports {
        let applied = match state.services.meta.record_capture_job_report(report).await {
            Ok(applied) => applied,
            Err(e) => {
                tracing::warn!(
                    host_id = %host_id,
                    job_id = %report.job_id,
                    error = %e,
                    "capture job report reconcile failed; host re-advertises next heartbeat",
                );
                false
            }
        };
        // One lookup serves both the ack-idempotency check below and the
        // dashboard mirror's `enable_job_id`. Keep PG errors distinct
        // from "row genuinely absent": an error must NEVER ack (the
        // report may still be landable — losing it would discard a
        // finished capture), while a genuinely-missing row can never
        // land and must ack or the host re-advertises forever.
        let row_lookup = state.services.meta.get_capture_job(report.job_id).await;
        let row = row_lookup.as_ref().ok().and_then(|r| r.as_ref());
        if report.terminal.is_some()
            && should_ack_capture_terminal(applied, &row_lookup, report.epoch)
        {
            acked_capture_jobs.push(report.job_id);
        }
        // Mirror onto the enable_jobs dashboard columns — best-effort,
        // cosmetic only (see `mirror_capture_progress_to_enable_job`'s
        // doc: it does NOT renew any enable-job claim). Gated on
        // `applied` so a fenced-off stale-epoch report can't overwrite
        // the live attempt's dashboard columns.
        if let (true, Some(row)) = (applied, &row) {
            let capture_phase = capture_job_stage_to_phase(report.stage);
            let warm_stage = report.progress.as_ref().and_then(|p| p.detail.as_deref());
            // Empty-string tails must NOT reach the COALESCE mirror: the
            // seed's dump/upload leg frames carry no hook output, and
            // `COALESCE('', old)` takes '' — the 2026-07-14 dev-brain
            // failure wiped the very hook tail the column exists to
            // preserve (the diagnosis survived only in host logs).
            let output_tail = report
                .progress
                .as_ref()
                .and_then(|p| p.log_tail.as_deref())
                .filter(|t| !t.is_empty());
            // ADR 0088 addendum: the capture timeline → the (previously
            // orphaned) `enable_jobs.warm_stages` column. Empty ⇒ None
            // ⇒ COALESCE keeps the last-known timeline.
            let warm_stages = report
                .progress
                .as_ref()
                .filter(|p| !p.warm_stages.is_empty())
                .and_then(|p| serde_json::to_value(&p.warm_stages).ok());
            if let Err(e) = state
                .services
                .meta
                .mirror_capture_progress_to_enable_job(
                    row.enable_job_id,
                    capture_phase.map(|p| p.as_str()),
                    warm_stage,
                    output_tail,
                    warm_stages.as_ref(),
                )
                .await
            {
                tracing::debug!(
                    host_id = %host_id,
                    job_id = %report.job_id,
                    error = %e,
                    "capture job dashboard mirror failed (non-fatal)",
                );
            }
        }
    }
    // This host's current `(job_id, epoch)` assignments — read
    // unconditionally every tick regardless of whether any capture jobs
    // exist fleet-wide (mirrors `list_prestaging_refs`'s posture).
    let capture_assignments = match state
        .services
        .meta
        .capture_assignments_for_host(host_id)
        .await
    {
        Ok(assignments) => Some(assignments),
        Err(e) => {
            tracing::debug!(host_id = %host_id, error = %e, "capture_assignments_for_host failed");
            None
        }
    };

    Ok(Json(HeartbeatResponse {
        server_time: state.services.clock.now_utc(),
        revoked_sessions: Vec::new(),
        enabled_images,
        prestage_images,
        live_bundles,
        acked_checkpoints,
        capture_assignments,
        acked_capture_jobs,
    }))
}

/// ADR 0084 P1b: map a `CaptureJobStage` to the `CapturePhase` the
/// `enable_jobs.capture_phase` dashboard column has always stored —
/// `Booting -> Boot`, `Warming -> Warm`, `Freezing -> Snapshot`;
/// `Assigned`/`Done`/`Failed` have no rendered phase (`None` leaves the
/// column at its last-known value via `COALESCE`).
/// ADR 0084 §A: may a host's TERMINAL capture-job report be acked (so the
/// host deletes its durable record and stops re-advertising)?
///
/// The rule: ack iff the report either landed (`applied`) or can NEVER
/// land —
///   - the row is genuinely gone, or
///   - the row's epoch has moved past the report's (reassigned away —
///     dead history, fenced off forever), or
///   - the row is already terminal at the same epoch (idempotent
///     re-advertise of an already-landed terminal).
///
/// A LOOKUP ERROR must never ack: the report may still be landable, and
/// acking it would make the host delete the only durable copy of a
/// (possibly successful) capture result on a PG blip. Conversely, the
/// superseded-epoch arm must ack, or a host whose attempt was reassigned
/// away re-advertises its fenced-off terminal every heartbeat FOREVER
/// and leaks the record file.
fn should_ack_capture_terminal(
    applied: bool,
    row_lookup: &Result<Option<engram_core::types::CaptureJobRow>, engram_core::error::MetaError>,
    report_epoch: i64,
) -> bool {
    if applied {
        return true;
    }
    match row_lookup {
        Err(_) => false,
        Ok(None) => true,
        Ok(Some(row)) => {
            row.epoch > report_epoch || (row.epoch == report_epoch && row.stage.is_terminal())
        }
    }
}

fn capture_job_stage_to_phase(
    stage: engram_core::types::CaptureJobStage,
) -> Option<engram_core::types::CapturePhase> {
    use engram_core::types::{CaptureJobStage, CapturePhase};
    match stage {
        CaptureJobStage::Booting => Some(CapturePhase::Boot),
        CaptureJobStage::Warming => Some(CapturePhase::Warm),
        CaptureJobStage::Freezing => Some(CapturePhase::Snapshot),
        CaptureJobStage::Assigned | CaptureJobStage::Done | CaptureJobStage::Failed => None,
    }
}

/// ADR 0015 M5: project enabled-image rows down to the wire
/// representation the host consumes — `(image_uri, manifest_digest)` plus,
/// since ADR 0021 P2, the base snapshot's disk manifest so the host can warm
/// the rootfs working set on NVMe (residency) before sessions restore.
fn enabled_image_refs_from_rows(
    rows: Vec<engram_core::types::EnabledImage>,
) -> Vec<EnabledImageRef> {
    rows.iter().filter_map(enabled_image_ref).collect()
}

/// ADR 0036 amendment (issue #538): the single-row half of
/// [`enabled_image_refs_from_rows`]'s projection, shared with
/// `enable_scanner`'s `Prestaging`-stage advertisement so the two build
/// paths can't drift — a row the enable scanner just captured (about to be
/// upserted) and a row already live in `enabled_images` project to the
/// SAME wire shape.
pub(crate) fn enabled_image_ref(row: &engram_core::types::EnabledImage) -> Option<EnabledImageRef> {
    // base_snapshot_disk_manifest is NOT NULL on a persisted row (migration
    // 0042); the Option here is only the build-then-stamp shape.
    // Defensively skip (rather than panic) the impossible None so one
    // malformed row can't break the whole advertisement.
    let Some(base_snapshot_disk_manifest) = row.base_snapshot_disk_manifest else {
        tracing::error!(
            image_uri = %row.image_uri,
            "enabled image has no base_snapshot_disk_manifest (NOT NULL invariant violated); not advertising",
        );
        return None;
    };
    // ADR 0022 Option A: base_snapshot_id is NOT NULL (migration 0038); same
    // defensive skip — the host needs it to key the per-template memfile
    // residency path.
    let Some(base_snapshot_id) = row.base_snapshot_id else {
        tracing::error!(
            image_uri = %row.image_uri,
            "enabled image has no base_snapshot_id (NOT NULL invariant violated); not advertising",
        );
        return None;
    };
    // ADR 0021 P2 (memory residency): nullable since migration 0049. `None`
    // for cold-boot backends (VZ) that capture a disk-only base snapshot —
    // advertise the row anyway; the host's prefetch warms only the disk
    // tier when memory is absent.
    Some(EnabledImageRef {
        image_uri: row.image_uri.clone(),
        manifest_digest: ManifestDigest(row.manifest_digest.clone()),
        base_snapshot_id,
        base_snapshot_disk_manifest,
        base_snapshot_memory_manifest: row.base_snapshot_memory_manifest,
        warm_peers: Vec::new(),
    })
}

/// ADR 0095: how many peer-fill seeds ride each image entry. Two: one
/// primary plus one alternate, so a requester's bounded second dial has
/// somewhere to go without waiting a heartbeat tick.
const WARM_PEER_SEEDS: usize = 2;

/// ADR 0095: stamp `warm_peers` onto image entries — fleet siblings
/// whose `ready_images` carry the digest, schedulable
/// ([`crate::placement::host_is_schedulable`]: Ready, uncordoned,
/// heartbeat-fresh, wire-compatible), with a dialable addr, excluding
/// the recipient. Pure so it unit-tests without PG.
///
/// Seed spread: candidates rotate by a hash of (recipient, digest), so
/// concurrent warmers fan out across the ready set instead of camping
/// on one seed, and a given recipient keeps stable seeds across ticks
/// (connection reuse) until the ready set changes.
pub(crate) fn attach_warm_peers(
    refs: &mut [EnabledImageRef],
    hosts: &[engram_core::types::host::HostRecord],
    recipient: engram_core::HostId,
    now: chrono::DateTime<chrono::Utc>,
) {
    use std::hash::{Hash, Hasher};
    let ttl = crate::placement::placement_ttl();
    for r in refs.iter_mut() {
        let mut candidates: Vec<(engram_core::HostId, &str)> = hosts
            .iter()
            .filter(|h| {
                h.id != recipient
                    && h.ready_images
                        .iter()
                        .any(|d| d == r.manifest_digest.as_str())
            })
            // `host_can_serve_chunks`, NOT `host_is_schedulable`: a
            // cordoned host mid-drain still serves reads happily, and
            // during a roll it's often the warmest seed available.
            .filter_map(|h| crate::placement::host_can_serve_chunks(h, now, ttl).map(|a| (h.id, a)))
            .collect();
        if candidates.is_empty() {
            r.warm_peers = Vec::new();
            continue;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        recipient.hash(&mut hasher);
        r.manifest_digest.as_str().hash(&mut hasher);
        let rot = (hasher.finish() as usize) % candidates.len();
        candidates.rotate_left(rot);
        r.warm_peers = candidates
            .into_iter()
            .take(WARM_PEER_SEEDS)
            .map(|(host_id, addr)| engram_protocol::heartbeat::PeerRef {
                host_id,
                addr: addr.to_string(),
            })
            .collect();
    }
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

// ---- POST /api/hosts/:id/capture-jobs/:job_id/claim ----

/// ADR 0084 P1b: the epoch a host is claiming — sent so the coordinator
/// can reject a stale claim (a host that raced a reassignment, or one
/// replaying an old `HeartbeatAck.capture_assignments` entry) instead of
/// silently handing out a fresh `CaptureJobSpec` for an epoch it no
/// longer owns.
#[derive(Deserialize)]
pub struct ClaimCaptureJobRequest {
    pub epoch: i64,
}

/// ADR 0084 §A: resolve the full dispatch for `(host_id, job_id, epoch)`
/// — `SandboxSpec` (same construction `capture_and_record_base_snapshot`
/// used to do directly), the image's optional `[warm]` hook, the
/// capture-time env resolved FRESH against `SecretStore` (fail-loud), and
/// the capture egress policy keyed by a synthetic session id derived
/// deterministically from `job_id` (stable across every claim of the
/// same job). Secrets ride only this authed response — never PG, never
/// the heartbeat, never the durable host-side record.
///
/// Validates the row is actually assigned to `host_id` at `epoch` and
/// non-terminal before doing any of that work. A resolve-env failure
/// (missing/unresolvable secret) is NOT retried by the host — it's a
/// deterministic misconfiguration, so THIS handler fails the job
/// directly (non-retryable) via a synthetic terminal report through the
/// same fenced `record_capture_job_report` path the heartbeat reconcile
/// uses, then returns the error to the host (which never gets a spec to
/// run, so it never starts an executor for this job/epoch).
pub async fn claim_capture_job(
    State(state): State<SharedState>,
    Path((host_id, job_id)): Path<(HostId, engram_core::types::CaptureJobId)>,
    Json(req): Json<ClaimCaptureJobRequest>,
) -> Result<Json<engram_core::types::capture_job::CaptureJobSpec>, ApiError> {
    let row = state
        .services
        .meta
        .get_capture_job(job_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("capture job {job_id} not found")))?;
    // ADR 0084 (c): `host_id` is `None` while WAITING for capacity — a
    // waiting job is never dispatched to any host, so a claim for one is a
    // stale/forged request. Only a job bound to exactly this host claims.
    if row.host_id != Some(host_id) {
        return Err(ApiError::BadRequest(format!(
            "capture job {job_id} is assigned to host {:?}, not {host_id}",
            row.host_id
        )));
    }
    if row.stage.is_terminal() {
        return Err(ApiError::Conflict(format!(
            "capture job {job_id} is already terminal ({})",
            row.stage
        )));
    }
    if row.epoch != req.epoch {
        return Err(ApiError::Conflict(format!(
            "capture job {job_id} claim requested epoch {}, current epoch is {}",
            req.epoch, row.epoch
        )));
    }

    let config = row.image_config.merged_with(&row.oci_defaults);
    let disk_manifest: engram_core::types::manifest::ManifestRef =
        row.disk_manifest.parse().map_err(|e| {
            ApiError::Internal(format!(
                "capture job {job_id}: stored disk_manifest {:?} failed to parse: {e}",
                row.disk_manifest
            ))
        })?;

    // Same construction `capture_and_record_base_snapshot` used to do
    // directly (issue #192: digest-pin `spec.image` so a moving tag can't
    // let the host's local OCI cache serve a different bake than the one
    // materialized; ADR 0057: capture boots with allow-all egress — a
    // trusted, ephemeral build step).
    let capture_uri = engram_oci::digest_pinned_uri(
        &row.image_uri,
        &engram_oci::Digest256(row.manifest_digest.clone()),
    );
    let capture_network = engram_core::types::image::NetworkPolicy {
        default: engram_core::types::image::NetworkDefault::Allow,
        allow_hosts: Vec::new(),
        allow_host_patterns: Vec::new(),
    };
    let spec = crate::api::sessions::cold_boot_spec(
        &capture_uri,
        &config,
        Some(disk_manifest),
        capture_network,
    );

    let warm_env = config
        .warm
        .as_ref()
        .map(|w| w.env.as_slice())
        .unwrap_or(&[]);
    let resolved_env = match crate::api::enabled_images::resolve_capture_env(
        &state,
        &row.image_uri,
        warm_env,
    )
    .await
    {
        Ok(env) => env,
        Err(e) => {
            // Deterministic misconfiguration — fail the job directly
            // (non-retryable) rather than hand the host a spec it
            // can't run, and rather than let the host report a
            // retryable transport-shaped failure for what is really
            // an operator fix (add the secret / fix the ref).
            let report = engram_core::types::CaptureJobReport {
                job_id,
                epoch: row.epoch,
                stage: engram_core::types::CaptureJobStage::Failed,
                progress: None,
                fc_snapshot_version: None,
                terminal: Some(engram_core::types::CaptureTerminalReport::Failed {
                    error: e.to_string(),
                    error_stage: "assigned".to_string(),
                    retryable: false,
                }),
            };
            if let Err(write_err) = state.services.meta.record_capture_job_report(&report).await {
                tracing::warn!(%job_id, error = %write_err, "claim_capture_job: failed to record env-resolve failure");
            }
            return Err(e);
        }
    };

    let capture_egress = config
        .warm
        .as_ref()
        .and_then(|w| w.network.as_ref())
        .and_then(|n| {
            crate::session_boot::assemble_capture_egress_policy(
                n,
                crate::session_boot::synthetic_capture_session_id(job_id),
            )
        });

    let cold_base_plan =
        crate::api::enabled_images::resolve_cold_base_plan(&state, host_id, &row, &config).await;

    tracing::info!(
        host_id = %host_id,
        %job_id,
        epoch = req.epoch,
        image_uri = %row.image_uri,
        cold_base_plan = ?cold_base_plan,
        "host claimed capture job",
    );
    Ok(Json(engram_core::types::capture_job::CaptureJobSpec {
        spec,
        warm: config.warm,
        resolved_env,
        capture_egress,
        cold_base_plan,
    }))
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

// ---- POST /api/sessions/:session_id/integration-asset (ADR 0056 Phase 4) ----

#[derive(Deserialize)]
pub struct IntegrationAssetReport {
    pub provider: String,
    pub asset_kind: String,
    /// `"action"` | `"asset"` — anything but `"asset"` is treated as `Action`.
    pub surface: String,
    pub data: serde_json::Value,
    #[serde(default)]
    pub fetchable_url: Option<String>,
    pub at: DateTime<Utc>,
}

/// The host-agent's egress proxy observed a response on a marked endpoint and
/// built an asset from the *real* response bytes (never a guest claim — the
/// proxy is the trusted observer). Append it as an `IntegrationAsset` session
/// event, the same `state.emit` path the mediated forge PR uses.
pub async fn integration_asset_ingest(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<IntegrationAssetReport>,
) -> Result<StatusCode, ApiError> {
    let surface = match req.surface.as_str() {
        "asset" => crate::state::AssetSurface::Asset,
        _ => crate::state::AssetSurface::Action,
    };
    let fetchable = req
        .fetchable_url
        .map(|url| crate::state::FetchableRef::External { url });
    state
        .emit(
            session_id,
            crate::state::SessionEvent::IntegrationAsset {
                provider: req.provider,
                asset_kind: req.asset_kind,
                surface,
                data: req.data,
                fetchable,
                at: req.at,
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- POST /api/hosts/:id/sessions/:session_id/inject/refresh (WS4) ----

#[derive(Deserialize)]
pub struct RefreshInjectRequest {
    /// Immutable host-side authority stored on the inject entry at boot.
    pub mint_source: engram_core::types::integration::CredentialMintSource,
    #[serde(default)]
    pub purpose: engram_core::types::integration::CredentialPurpose,
}

#[derive(Serialize)]
pub struct RefreshInjectResponse {
    /// The header the credential renders into (unchanged across a refresh, but
    /// returned so the proxy needn't assume the scheme).
    pub header_name: String,
    /// The freshly rendered header value (e.g. `Bearer ghs_…`) — the proxy
    /// substitutes it verbatim. Host-side only; never reaches the guest.
    pub secret: String,
    /// When the new credential expires (drives the proxy's next refresh).
    pub expires_at: DateTime<Utc>,
}

/// WS4: the egress proxy's minted inject credential (a GitHub App installation
/// token, ~1h TTL) is nearing expiry on a long-lived session. Re-mint it against
/// the session's bound capabilities — the same scope `resolve_inject_entries`
/// used at boot — and hand back the fresh header + TTL. This closes the
/// campaign's reads-401/writes-succeed asymmetry: the boot-time inject token was
/// minted ONCE and went stale ~1h later while the askpass write path re-minted
/// per call. Fail-loud (4xx/5xx) so the proxy keeps the stale secret rather than
/// injecting an empty header; the mint itself is cached + single-flighted
/// server-side (`GitHubApp::mint_basic`), so a refresh burst re-mints once.
pub async fn refresh_inject(
    State(state): State<SharedState>,
    Path((host_id, session_id)): Path<(HostId, SessionId)>,
    Json(req): Json<RefreshInjectRequest>,
) -> Result<Json<RefreshInjectResponse>, ApiError> {
    let (header_name, secret, expires_at) = if req.purpose.as_str() == "api" {
        let (header, expires_at) =
            crate::session_boot::refresh_inject_header(&state, session_id, &req.mint_source)
                .await
                .ok_or_else(|| {
                    ApiError::Internal(format!(
                        "inject refresh for {:?} on session {session_id} could not be minted",
                        req.mint_source
                    ))
                })?;
        (header.name, header.value, expires_at)
    } else if let engram_core::types::integration::CredentialMintSource::Connection {
        connection_id,
        ..
    } = &req.mint_source
    {
        let (token, expires_at) = crate::session_boot::mint_remote_connection_credential(
            session_id,
            connection_id,
            req.purpose.clone(),
        )
        .await
        .ok_or_else(|| ApiError::Internal("host credential could not be minted".into()))?;
        (String::new(), token, expires_at)
    } else {
        return Err(ApiError::BadRequest(
            "credential purpose is invalid for source".into(),
        ));
    };
    tracing::debug!(
        %host_id,
        %session_id,
        source = ?req.mint_source,
        purpose = ?req.purpose,
        %expires_at,
        "minted host-side connection credential",
    );
    Ok(Json(RefreshInjectResponse {
        header_name,
        secret,
        expires_at,
    }))
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
    let outcome = live_manifest_publish_core(&state, host_id, &req).await?;
    Ok(Json(LiveManifestPublishResponse { outcome }))
}

/// The pure store-level core of [`live_manifest_publish`] (ADR 0098
/// R-CoSim, the run_once pattern applied to handlers): the HTTP wrapper
/// thins to extractor + JSON, this holds the real logic so the boundary
/// simulator (`engram-dst-cosim`) can drive the exact coordinator code
/// path the host-agent's `CoordControlPlane::publish_live_manifest` hits.
/// Zero behavior change.
/// The store-level core of the [`register`] handler's rehydration list (ADR
/// 0098 R-CoSim, the run_once pattern applied to handlers). Returns the
/// VM-resident sandboxes PG has bound to `host_id` — every
/// `reserves_host_memory` state (Active AND rung-parked Evicting survivors,
/// per session 731df805) — with the effective disk manifest the host rebuilds
/// from. The boundary simulator's register-rehydrate leg drives THIS exact
/// listing, so the co-simulated host re-serves precisely the devices the real
/// coordinator would name.
pub async fn register_rehydrate_list_core(
    state: &SharedState,
    host_id: HostId,
) -> Result<Vec<RehydrateSandboxRef>, ApiError> {
    let rows = state
        .services
        .meta
        .list_resident_sandboxes_on_host_with_disk_manifest(host_id)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(session_id, sandbox_id, manifest)| RehydrateSandboxRef {
            session_id,
            sandbox_id,
            disk_manifest_id: manifest.as_ref().map(|m| m.manifest_id),
            disk_manifest_version: manifest.as_ref().map(|m| m.version),
        })
        .collect())
}

/// The ADR 0090 quarantined-survivor advertise arm of the heartbeat,
/// extracted per the run_once pattern so the boundary co-simulator
/// (`engram-dst-cosim`) drives the REAL reaction to a host's quarantine
/// advertise — the seam the 2026-07-21 8174b7aa livelock lived in (the
/// cosim previously wrote host liveness straight to the store, so the
/// advertise → enqueue → skip loop was structurally invisible to it).
/// For each survivor the session still owns, enqueue the keyed
/// quarantine `evict_local`; the pipeline's convergence guarantee (see
/// the heartbeat handler's comment) is what keeps this 5s-cadence
/// enqueue loop-free.
pub async fn quarantined_survivor_advertise_core(
    state: &SharedState,
    host_id: HostId,
    survivors: &[engram_protocol::heartbeat::QuarantinedSurvivor],
) {
    for q in survivors {
        match state.services.meta.get_session(q.session_id).await {
            Ok(s) if s.sandbox_id == Some(q.sandbox_id) => {
                match crate::session_ops::enqueue(
                    state,
                    q.session_id,
                    engram_core::types::session_op::OpKind::Evict,
                    serde_json::json!({
                        "target": "idle",
                        "allow_park": false,
                        "nominated": false,
                        // Quarantine flavor: the survivor's disk is unserved, so
                        // the evict verb bounds each capture attempt and, past
                        // the fast-retry budget, parks the op on a slow retry
                        // cadence until the host's rehydrate retry re-serves
                        // the disk (2026-08-02 durability-rollback RCA — the
                        // old exhaustion arm destroyed the VM and rewound past
                        // acked writes).
                        "quarantine": true,
                        // Pin the op to the advertised sandbox: the slow lane
                        // can outlive a relocation, and a stale wake-up must
                        // no-op instead of evicting the session's NEW, healthy
                        // sandbox.
                        "sandbox_id": q.sandbox_id,
                    }),
                    Some(&format!("adr0090-quarantine:{}", q.sandbox_id)),
                )
                .await
                {
                    Ok(engram_core::types::session_op::EnqueueOutcome::Duplicate) => {}
                    Ok(_) => {
                        tracing::warn!(
                            host_id = %host_id,
                            session_id = %q.session_id,
                            sandbox_id = %q.sandbox_id,
                            "quarantined survivor advertised — enqueued evict_local \
                             (capture + relocate; ADR 0090)",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            host_id = %host_id,
                            session_id = %q.session_id,
                            error = %e,
                            "quarantined-survivor evict enqueue failed; retried next heartbeat",
                        );
                    }
                }
            }
            // Session moved on (relocated / terminal) or unknown — the
            // host's quarantine entry clears when the sandbox is
            // destroyed; nothing to drive here.
            _ => {}
        }
    }
}

pub async fn live_manifest_publish_core(
    state: &SharedState,
    host_id: HostId,
    req: &LiveManifestPublishRequest,
) -> Result<LiveManifestPublishOutcome, ApiError> {
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
            Ok(LiveManifestPublishOutcome::Applied)
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
            Ok(LiveManifestPublishOutcome::Stale)
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
    let owned = sandbox_ownership_core(&state, session_id, sandbox_id).await?;
    Ok(Json(SandboxOwnershipResponse { owned }))
}

/// The store-level core of [`sandbox_ownership`] (ADR 0098 R-CoSim, the
/// run_once pattern applied to handlers). Zero behavior change — the
/// ADR 0092 non-terminal predicate lives here so the boundary simulator
/// drives the exact ownership answer the host-agent's reconcile tick reads.
pub async fn sandbox_ownership_core(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> Result<bool, ApiError> {
    // ADR 0092 hardening: a terminal row owns nothing, even if its
    // `sandbox_id` column still carries the binding — a create that
    // failed AFTER the VM spawned flips the session Failed and leans on
    // the teardown reconciler to reap the live VM; answering
    // `owned=true` here kept those orphans alive (and their guest
    // memory pinned) indefinitely. Mirrors `session_owning_sandbox`'s
    // non-terminal predicate.
    match state.services.meta.get_session(session_id).await {
        Ok(s) => Ok(s.sandbox_id == Some(sandbox_id) && !s.status.is_terminal()),
        Err(engram_core::MetaError::NotFound) => Ok(false),
        Err(e) => Err(ApiError::Internal(format!("get_session: {e}"))),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SandboxOwnershipResponse {
    pub owned: bool,
}

/// ADR 0090: the teardown reconciler's unknown-binding form — "does ANY
/// session own sandbox Y on this host?" A fresh host-agent generation
/// whose NBD rehydrate failed has no local binding for a pidfd-reattached
/// survivor; before the reconciler may count an orphan strike it must ask
/// here. Returns the owning session id (non-terminal states only, incl.
/// `host_lost`) so the host can also repopulate its binding table.
pub async fn sandbox_owner(
    State(state): State<SharedState>,
    Path((host_id, sandbox_id)): Path<(HostId, SandboxId)>,
) -> Result<Json<SandboxOwnerResponse>, ApiError> {
    let session_id = sandbox_owner_core(&state, host_id, sandbox_id).await?;
    Ok(Json(SandboxOwnerResponse { session_id }))
}

/// The store-level core of [`sandbox_owner`] (ADR 0098 R-CoSim, the
/// run_once pattern applied to handlers). Zero behavior change.
pub async fn sandbox_owner_core(
    state: &SharedState,
    host_id: HostId,
    sandbox_id: SandboxId,
) -> Result<Option<SessionId>, ApiError> {
    state
        .services
        .meta
        .session_owning_sandbox(host_id, sandbox_id)
        .await
        .map_err(|e| ApiError::Internal(format!("session_owning_sandbox: {e}")))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SandboxOwnerResponse {
    pub session_id: Option<SessionId>,
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::SessionMode;
    use engram_core::types::Session;
    use engram_core::types::SessionState;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_for_session(session: Session) -> (SharedState, Arc<MiniMeta>, TempDir) {
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
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, meta, local)
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
            last_event_at: None,
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// ADR 0092 hardening: a terminal session whose `sandbox_id` column
    /// still carries the binding must answer `owned=false`. Before this,
    /// a create that failed after the VM spawned (session flipped Failed,
    /// binding never cleared) kept its orphan alive forever: the host's
    /// teardown reconciler asked here, got `owned=true`, and never
    /// counted an orphan strike.
    #[tokio::test]
    async fn sandbox_ownership_denies_terminal_sessions() {
        let sid = engram_core::SessionId::new();
        let sandbox = SandboxId::new();
        for (status, want) in [
            (SessionState::Active, true),
            (SessionState::Idle, true),
            (SessionState::Failed, false),
            (SessionState::Completed, false),
            (SessionState::Dead, false),
        ] {
            let (state, _meta, _tmp) =
                build_state_for_session(session_with_status(sid, sandbox, status));
            let Json(resp) = sandbox_ownership(
                State(state),
                Path((engram_core::HostId::new(), sid, sandbox)),
            )
            .await
            .expect("handler");
            assert_eq!(resp.owned, want, "status {status:?}: expected owned={want}");
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

    /// ADR 0068 / issue #531 regression (PR #564): a heartbeat whose
    /// `touch_host_heartbeat` persist fails must 5xx AND must never
    /// reach `reconcile_host` for this tick — "Postgres is the
    /// authority" means reconcile can only act on a heartbeat that
    /// actually landed. Proves the EARLY RETURN, not just "no flip
    /// happened" — this fixture's `apply_missing_sandbox_strikes` never
    /// flips regardless (the trait's no-op default), so a weaker
    /// "assert no flip" test would pass even if the persist/reconcile
    /// order were swapped back. `reconcile_probe_calls` counts entry
    /// into `list_resident_sandbox_assignments_on_host` — the call
    /// `Reconciler::reconcile_with_deps` makes on every tick it
    /// actually runs — so it distinguishes "reconcile ran and found
    /// nothing" from "reconcile never ran".
    #[tokio::test]
    async fn heartbeat_persist_failure_skips_reconcile_this_tick() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let session_id = engram_core::SessionId::new();
        let mut session = session_with_status(session_id, sandbox_id, SessionState::Active);
        session.host_id = Some(host_id);
        let (state, meta, _local) = build_state_for_session(session);

        let hb_json = serde_json::json!({
            "capacity": { "total_mib": 1024, "used_mib": 0, "running_sandboxes": 1 },
            "running_sandboxes": [sandbox_id],
        });

        // Tick 1: force the persist to fail.
        *meta.fail_next_heartbeat_persist.lock() = true;
        let hb: HeartbeatRequest =
            serde_json::from_value(hb_json.clone()).expect("deserialize heartbeat");
        let result = heartbeat(State(state.clone()), Path(host_id), Json(hb)).await;
        assert!(
            result.is_err(),
            "a heartbeat whose persist fails must 5xx, not silently ack"
        );
        assert_eq!(
            *meta.reconcile_probe_calls.lock(),
            0,
            "reconcile must not run this tick — the persist never landed"
        );

        // Tick 2: persist succeeds, so reconcile DOES run — proves the
        // zero count above is specifically caused by the persist
        // failure, not some other reason this fixture never reconciles.
        let hb2: HeartbeatRequest = serde_json::from_value(hb_json).expect("deserialize heartbeat");
        let result2 = heartbeat(State(state.clone()), Path(host_id), Json(hb2)).await;
        assert!(
            result2.is_ok(),
            "a heartbeat with a healthy persist must succeed: {:?}",
            result2.err()
        );
        // ADR 0073: the disagreement-alarm liveness check (added by the
        // binding-epoch work) ALSO probes `list_active_sandbox_assignments_
        // on_host` after a successful persist, so the exact count is ≥1 (not
        // exactly 1 as on pre-binding-epoch main). The load-bearing assertion
        // is the 0-vs-nonzero split: tick 1 (persist failed) probed 0, tick 2
        // (persist succeeded) probed at least once — proving the early return.
        assert!(
            *meta.reconcile_probe_calls.lock() >= 1,
            "reconcile must run once the persist succeeds"
        );
    }

    /// 2026-07-13 incident regression: the ADR 0090 quarantined-survivor
    /// arm fires on EVERY 5s heartbeat, and `session_ops` dedup keys
    /// ONLY on the idempotency key — a key-less enqueue inserts a fresh
    /// queued row per heartbeat (423 piled up behind one wedged evict in
    /// prod). Pin that re-adverts collapse to ONE keyed row. The seeded
    /// running evict keeps the lane busy so the first advert's row stays
    /// `queued` (never claimed/driven) and the second advert must dedup
    /// against it.
    #[tokio::test]
    async fn quarantined_survivor_readverts_dedup_to_one_op() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let session_id = engram_core::SessionId::new();
        let mut session = session_with_status(session_id, sandbox_id, SessionState::Active);
        session.host_id = Some(host_id);
        let (state, meta, _local) = build_state_for_session(session);
        meta.ops
            .seed_running(session_id, engram_core::types::session_op::OpKind::Evict);

        let hb_json = serde_json::json!({
            "capacity": { "total_mib": 1024, "used_mib": 0, "running_sandboxes": 1 },
            "running_sandboxes": [sandbox_id],
            "quarantined_survivors": [
                { "sandbox_id": sandbox_id, "session_id": session_id },
            ],
        });
        for tick in 0..2 {
            let hb: HeartbeatRequest =
                serde_json::from_value(hb_json.clone()).expect("deserialize heartbeat");
            let result = heartbeat(State(state.clone()), Path(host_id), Json(hb)).await;
            assert!(result.is_ok(), "heartbeat tick {tick}: {:?}", result.err());
        }

        let keyed: Vec<_> = meta
            .ops
            .all()
            .into_iter()
            .filter(|o| {
                o.idempotency_key.as_deref()
                    == Some(format!("adr0090-quarantine:{sandbox_id}").as_str())
            })
            .collect();
        assert_eq!(
            keyed.len(),
            1,
            "re-advertised quarantined survivor must dedup to one keyed evict op, got {keyed:#?}",
        );
    }

    /// ADR 0084 §A: the terminal-report ack rule. The two failure modes
    /// this pins: (a) a superseded-epoch terminal MUST ack, or the old
    /// host re-advertises its fenced-off record every heartbeat forever;
    /// (b) a PG lookup error must NEVER ack, or a blip deletes the only
    /// durable copy of a finished capture before it landed.
    #[test]
    fn capture_terminal_ack_rule() {
        use engram_core::error::MetaError;
        use engram_core::types::capture_job::{CaptureJobRow, CaptureJobStage};

        fn row(epoch: i64, stage: CaptureJobStage) -> CaptureJobRow {
            let now = chrono::Utc::now();
            CaptureJobRow {
                id: engram_core::types::CaptureJobId::new(),
                enable_job_id: uuid::Uuid::new_v4(),
                image_uri: "reg.test/img:tag".into(),
                manifest_digest: "0".repeat(64),
                disk_manifest: "manifest".into(),
                image_config: Default::default(),
                oci_defaults: Default::default(),
                host_id: Some(HostId::new()),
                mem_budget_mib: 2_048,
                cpu_budget_vcpus: 2,
                waiting_since: None,
                epoch,
                stage,
                stage_started_at: now,
                stage_progress: None,
                last_progress_at: now,
                attempts: 1,
                retryable: None,
                error: None,
                error_stage: None,
                fc_snapshot_version: None,
                result_bincode: None,
                created_at: now,
                updated_at: now,
            }
        }

        // Applied: always ack, whatever the lookup said.
        assert!(should_ack_capture_terminal(
            true,
            &Err(MetaError::Migration("pg down".into())),
            1
        ));
        // Lookup error, not applied: never ack (retry next heartbeat).
        assert!(!should_ack_capture_terminal(
            false,
            &Err(MetaError::Migration("pg down".into())),
            1
        ));
        // Row gone: the report can never land — ack.
        assert!(should_ack_capture_terminal(false, &Ok(None), 1));
        // Superseded epoch (reassigned away): ack, even though the live
        // row is non-terminal — the immortal-re-advertise regression.
        assert!(should_ack_capture_terminal(
            false,
            &Ok(Some(row(2, CaptureJobStage::Booting))),
            1
        ));
        // Same epoch, already terminal: idempotent re-advertise — ack.
        assert!(should_ack_capture_terminal(
            false,
            &Ok(Some(row(1, CaptureJobStage::Failed))),
            1
        ));
        // Same epoch, NON-terminal, not applied (transient write failure):
        // don't ack — the report is still landable.
        assert!(!should_ack_capture_terminal(
            false,
            &Ok(Some(row(1, CaptureJobStage::Warming))),
            1
        ));
        // A row somehow BEHIND the report's epoch (shouldn't happen —
        // epochs only move forward): not ours to discard.
        assert!(!should_ack_capture_terminal(
            false,
            &Ok(Some(row(1, CaptureJobStage::Booting))),
            2
        ));
    }

    /// ADR 0095: `attach_warm_peers` seed-selection matrix.
    mod warm_peers {
        use super::*;
        use chrono::Utc;
        use engram_core::types::host::{
            HostCapacity, HostMetadata, HostRecord, HostStatus, HostUtilization,
        };
        use engram_core::HostId;
        use engram_protocol::heartbeat::ManifestDigest;

        fn hid(id: u128) -> HostId {
            HostId(uuid::Uuid::from_u128(id))
        }

        fn host(id: u128, ready: &[&str]) -> HostRecord {
            HostRecord {
                id: hid(id),
                hostname: format!("h{id}"),
                cloud_metadata: HostMetadata::default(),
                capacity: HostCapacity {
                    total_gb: 0,
                    used_gb: 0,
                    total_mib: 0,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: HostUtilization::default(),
                status: HostStatus::Ready,
                last_heartbeat_at: Utc::now(),
                host_addr: Some(format!("http://10.0.0.{id}:9101")),
                ready_images: ready.iter().map(|s| s.to_string()).collect(),
                current_bundles: Vec::new(),
                sandbox_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0, // 0 = not-yet-reported, tolerated
                stages_images: true,
                capabilities: Default::default(),
                lease_expires_at: None,
                lease_state: Default::default(),
                lease_epoch: 0,
            }
        }

        fn image_ref(digest: &str) -> EnabledImageRef {
            EnabledImageRef {
                image_uri: "localhost/x:1".into(),
                manifest_digest: ManifestDigest::new(digest.to_string()),
                base_snapshot_id: engram_core::SnapshotId::new(),
                base_snapshot_disk_manifest: engram_core::types::manifest::ManifestRef {
                    manifest_id: uuid::Uuid::nil(),
                    version: 1,
                },
                base_snapshot_memory_manifest: None,
                warm_peers: Vec::new(),
            }
        }

        const D: &str = "sha256:aaa";

        #[test]
        fn seeds_ready_holders_excluding_recipient_capped_at_two() {
            let hosts = vec![
                host(1, &[D]),
                host(2, &[D]),
                host(3, &[D]),
                host(4, &["sha256:other"]),
            ];
            let mut refs = vec![image_ref(D)];
            attach_warm_peers(&mut refs, &hosts, hid(1), Utc::now());
            let peers = &refs[0].warm_peers;
            assert_eq!(peers.len(), 2, "capped at {WARM_PEER_SEEDS}");
            assert!(
                peers.iter().all(|p| p.host_id != hid(1)),
                "recipient must never seed itself"
            );
            assert!(
                peers.iter().all(|p| p.host_id != hid(4)),
                "a host without the digest must not seed it"
            );
        }

        #[test]
        fn cordoned_host_still_seeds_but_dead_and_skewed_do_not() {
            let mut cordoned = host(2, &[D]);
            cordoned.cordoned = true; // mid-drain: warmest seed there is
            let mut dead = host(3, &[D]);
            dead.last_heartbeat_at = Utc::now() - chrono::Duration::hours(1);
            let mut skewed = host(4, &[D]);
            skewed.wire_version = engram_protocol::WIRE_VERSION - 1;
            let mut addrless = host(5, &[D]);
            addrless.host_addr = None;
            let hosts = vec![cordoned, dead, skewed, addrless];
            let mut refs = vec![image_ref(D)];
            attach_warm_peers(&mut refs, &hosts, hid(1), Utc::now());
            let peers = &refs[0].warm_peers;
            assert_eq!(
                peers.iter().map(|p| p.host_id).collect::<Vec<_>>(),
                vec![hid(2)],
                "cordoned seeds; dead / wire-skewed / addr-less never do"
            );
        }

        #[test]
        fn no_candidates_means_empty_hints_never_self() {
            let hosts = vec![host(1, &[D])]; // only the recipient itself
            let mut refs = vec![image_ref(D)];
            attach_warm_peers(&mut refs, &hosts, hid(1), Utc::now());
            assert!(refs[0].warm_peers.is_empty());
        }

        #[test]
        fn rotation_spreads_recipients_across_seeds() {
            let hosts: Vec<HostRecord> = (2..=5).map(|i| host(i, &[D])).collect();
            // Different recipients should not all camp on the same
            // first seed. With 4 candidates and a hash rotation, at
            // least two distinct primaries must appear across a set of
            // recipients (deterministic given fixed UUIDs).
            let primaries: std::collections::HashSet<_> = (10u128..30)
                .map(|r| {
                    let mut refs = vec![image_ref(D)];
                    attach_warm_peers(&mut refs, &hosts, hid(r), Utc::now());
                    refs[0].warm_peers[0].host_id
                })
                .collect();
            assert!(
                primaries.len() >= 2,
                "hash rotation must spread primaries, got {primaries:?}"
            );
        }
    }
}
