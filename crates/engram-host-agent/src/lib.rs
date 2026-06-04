//! Per-host daemon. Wraps a [`SandboxBackend`], composes the
//! chunked-OCI image cache / tiered chunk resolver / egress proxy /
//! NBD pool, runs the snapshot manager and resource governor, and
//! heartbeats the coordinator with capacity + local-snapshot state.
//!
//! Phase 1 wires create/exec/destroy through the SandboxBackend trait —
//! currently `engram-sandbox-process` for fast dev loops, with
//! `engram-sandbox-firecracker` filling in the production path. Phase 2
//! lights up the snapshot manager. Phase 3 turns heartbeats into a real
//! gRPC channel; today they go through an in-process trait object so
//! the `--mode=all` single-binary path works.

use std::path::PathBuf;
use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::{CloudBackend, SandboxBackend};
use engram_core::SandboxId;

use crate::image_cache::ImageCache;

pub mod admin_handler;
pub mod blob;
pub mod bundles;
pub mod checkpoint;
pub mod config;
pub mod coord_client;
pub mod disk_daemon;
pub mod egress;
pub mod grpc_server;
pub mod harness;
pub mod host_client;
pub use host_client::LocalHostClient;
pub mod heartbeat;
pub mod idle_evictor;
pub mod image_cache;
pub mod image_prefetch;
pub mod live_attach;
pub mod metrics;
pub mod orphan_reap;
pub mod pooled_backend;
pub mod proxy_shell;
pub mod resource;
pub mod shutdown;
pub mod snapshot;
pub mod trace_scope;

pub use config::HostAgentConfig;

/// ADR 0014 M1.12 / ADR 0020: materialize the host's 16 MiB empty ext4
/// stub harness (idempotent — rebuilt only if missing or wrong-sized).
/// Both the warm-pool restore path and ADR 0020's base-snapshot capture
/// attach it as the harness drive; `swap_harness_drive` re-points it at
/// the session's real harness at lease/restore time. Returned path is
/// canonicalized so the FC drive symlink resolves. Lives in the lib so
/// both the `engram-host-agent` (mode=host) and `engram-coordinator`
/// (mode=all) binaries wire the same stub.
pub async fn ensure_stub_harness(path: &std::path::Path) -> Result<PathBuf, String> {
    const STUB_SIZE_BYTES: u64 = 16 * 1024 * 1024;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir stub parent {}: {e}", parent.display()))?;
    }
    let needs_build = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len() != STUB_SIZE_BYTES,
        Err(_) => true,
    };
    if needs_build {
        let scratch = tempfile::tempdir().map_err(|e| format!("stub tempdir: {e}"))?;
        use engram_image_builder::{Ext4Packer, Mke2fsPacker};
        Mke2fsPacker::default()
            .pack(scratch.path(), path, STUB_SIZE_BYTES)
            .await
            .map_err(|e| format!("mke2fs stub harness: {e}"))?;
    }
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("canonicalize stub harness {}: {e}", path.display()))
}

pub struct HostAgent {
    pub cfg: HostAgentConfig,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub cloud: Arc<dyn CloudBackend>,
    /// ADR 0006: local egress proxy. `None` keeps egress unfiltered
    /// (dev / explicit opt-out via `--egress-proxy-port=0`).
    pub egress: Option<Arc<egress::HostEgress>>,
    /// ADR 0007: chunk store + per-host materialization root. `None`
    /// keeps the legacy OCI-pulled `rootfs.ext4` path active (chunked
    /// images skip the chunk-store resolve and use the cached file
    /// directly).
    pub chunk_store: Option<(ChunkStore, PathBuf)>,
    /// Per-host OCI image cache. `None` means the host-agent can't
    /// pull images by URI — sessions must arrive with
    /// `spec.rootfs_source` set already (the dev-only path).
    /// Production multi-host topologies must set this.
    pub image_cache: Option<ImageCache>,
    /// ADR 0007 #3a: NVMe-backed chunk cache. Optional; wired in
    /// production to amortise chunk reads across manifests.
    pub chunk_cache: Option<ChunkCache>,
    /// ADR 0007 Phase 4: pool of `/dev/nbdN` device paths the
    /// daemon allocates from when serving chunked rootfs disks.
    /// `None` keeps the legacy materialize-to-file path active.
    pub nbd_pool: Option<Arc<disk_daemon::NbdSlotAllocator>>,
    /// ADR 0009 §6: typed handle on the FC backend (when this host
    /// uses Firecracker) so the live-attach pass at startup can
    /// invoke `reattach_sandbox`. The `Arc<dyn SandboxBackend>` in
    /// `sandbox` can't be downcast to a concrete type (the trait
    /// doesn't extend `Any`), so callers that want live-attach
    /// must wire this separately. `None` skips reattach entirely
    /// (clean-slate startup; matches today's behaviour).
    pub fc_for_reattach: Option<Arc<engram_sandbox_firecracker::FirecrackerBackend>>,
    /// ADR 0007 Phase 5: this host's stable `HostId`. Stamped on
    /// snapshots' `trace_host_hint` (so cross-host restore knows
    /// which trace to prefault) AND passed to the UFFD handler
    /// as `--publish-trace-host`. `None` generates a fresh id at
    /// run time (the pre-Phase-5 default).
    pub host_id: Option<engram_core::HostId>,
}

impl HostAgent {
    pub fn new(
        cfg: HostAgentConfig,
        sandbox: Arc<dyn SandboxBackend>,
        cloud: Arc<dyn CloudBackend>,
    ) -> Self {
        Self {
            cfg,
            sandbox,
            cloud,
            egress: None,
            chunk_store: None,
            image_cache: None,
            chunk_cache: None,
            nbd_pool: None,
            host_id: None,
            fc_for_reattach: None,
        }
    }

    /// ADR 0009 §6: register the concrete FC backend for the
    /// startup reattach pass. Optional — only meaningful when
    /// `--sandbox-backend=firecracker` AND `ENGRAM_LIVE_ATTACH=1`.
    /// When unset, the host-agent starts clean-slate (reconcile
    /// then flips orphaned sessions per §3).
    pub fn with_fc_reattach(
        mut self,
        fc: Arc<engram_sandbox_firecracker::FirecrackerBackend>,
    ) -> Self {
        self.fc_for_reattach = Some(fc);
        self
    }

    /// Set this host's stable `HostId`. Pair with the same id
    /// stamped on `FirecrackerConfig.host_id` at backend
    /// construction — both sides need to agree so the trace
    /// `traces/<manifest_id>/<host_id>.json` keying is consistent.
    pub fn with_host_id(mut self, id: engram_core::HostId) -> Self {
        self.host_id = Some(id);
        self
    }

    /// Attach a `ChunkCache`. Optional; layers on top of
    /// `with_chunk_store` to amortise chunk reads across manifests
    /// (canonical-base images, forks).
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Attach a `/dev/nbdN` slot allocator. When set + a chunk
    /// store + chunk cache are also wired, `create()` spawns the
    /// NBD daemon to serve chunked rootfs disks instead of
    /// materializing them to single files. Linux-only at runtime.
    pub fn with_nbd_pool(mut self, pool: Arc<disk_daemon::NbdSlotAllocator>) -> Self {
        self.nbd_pool = Some(pool);
        self
    }

    /// Attach a local egress proxy. The host-agent will route every
    /// inbound `notify_session_policy` to this proxy's registry,
    /// unregister sandboxes on `destroy`, and expose the CA cert
    /// PEM to substrate-building code paths.
    pub fn with_egress(mut self, egress: Arc<egress::HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Attach a chunk store + per-host materialization directory.
    /// The PooledBackend uses these to resolve chunked image
    /// manifests to per-host materialized rootfs files.
    pub fn with_chunk_store(mut self, chunk_store: ChunkStore, materialize_dir: PathBuf) -> Self {
        self.chunk_store = Some((chunk_store, materialize_dir));
        self
    }

    /// Attach a per-host OCI image cache. Sessions with `image_uri`
    /// set route through this cache (pull on miss, hit on subsequent
    /// references). Required for multi-host production where the
    /// coordinator hands out images by URI.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Run the host agent's background loops until shutdown.
    ///
    /// Phase 3a: if `coordinator_endpoint` is set, runs the WS dialer
    /// against that coordinator until ctrl-c (reconnecting on drops).
    /// Otherwise stays idle until ctrl-c — useful for `engram-host-agent`
    /// in standalone dev where the binary just hosts a local backend
    /// without phoning home.
    pub async fn run(self) -> Result<(), HostAgentError> {
        tracing::info!(?self.cfg.work_dir, "host-agent starting");
        let _ = self.cloud.host_metadata().await;

        // ADR 0009 §6: live-VM reattach pass. Runs once at startup,
        // before connecting to coord, so reattached sandboxes show
        // up in the very first heartbeat's `running_sandboxes`
        // field — coord sees them as continuously-present and
        // doesn't strike-out / flip the owning sessions. Gated by
        // `ENGRAM_LIVE_ATTACH` and only meaningful when the
        // concrete FC backend was wired via `with_fc_reattach`.
        if std::env::var("ENGRAM_LIVE_ATTACH").ok().as_deref() == Some("1") {
            if let Some(fc) = self.fc_for_reattach.as_ref() {
                match live_attach::reattach_pass(&self.cfg.work_dir, fc).await {
                    Ok(report) => {
                        tracing::info!("{}", report.summary());
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "live-attach pass failed; continuing with clean-slate startup"
                        );
                    }
                }
            } else {
                tracing::info!(
                    "ENGRAM_LIVE_ATTACH=1 set but no FC backend registered for reattach \
                     (call with_fc_reattach to enable); skipping reattach pass"
                );
            }
        }

        if let Some(coord_url) = self.cfg.coordinator_endpoint.clone() {
            // Re-use the host_id that was stamped on the FC
            // backend at construction time, so trace replay keys
            // line up. Falls back to a fresh id when the caller
            // didn't set one — that's the pre-Phase-5 behaviour
            // (no trace replay).
            let host_id = self.host_id.unwrap_or_default();
            tracing::info!(
                host_id = %host_id,
                coordinator = %coord_url,
                "dialing coordinator",
            );
            // Wrap the underlying SandboxBackend in a PooledBackend so
            // session creation can attach the egress / chunk-store /
            // image-cache state without burdening the underlying VZ /
            // FC drivers with knowledge of those subsystems.
            let pooled = {
                let mut p = pooled_backend::PooledBackend::new(self.sandbox.clone());
                if let Some(egress) = self.egress.clone() {
                    p = p.with_egress(egress);
                }
                if let Some((cs, dir)) = self.chunk_store.clone() {
                    p = p.with_chunk_store(cs, dir);
                }
                if let Some(cache) = self.chunk_cache.clone() {
                    p = p.with_chunk_cache(cache);
                }
                if let Some(ic) = self.image_cache.clone() {
                    // ADR 0008 Phase 5: also feed the image cache's
                    // OciClient into the pooled backend so chunked-
                    // OCI images can fault chunks from the registry
                    // via the tiered resolver path.
                    p = p.with_oci_client(ic.oci_client());
                    p = p.with_image_cache(ic);
                }
                if let Some(pool) = self.nbd_pool.clone() {
                    p = p.with_nbd_pool(pool);
                }
                // ADR 0016 Phase B: wire the coord-bound
                // live-manifest publisher. The publisher's drain
                // task is owned by `p` (via
                // `LiveManifestPublisherHandle`), so it dies with
                // the host-agent process.
                let publisher_coord = coord_client::CoordClient::new(
                    coord_url.clone(),
                    self.cfg.coordinator_token.clone(),
                );
                p = p.with_live_manifest_coord_publisher(publisher_coord, host_id);
                // ADR 0028 Fix A: checkpoint chains (rolling memory
                // images + durable records) live under the work dir.
                // Gated on (a) a chunk store being wired — nothing
                // durable to chain without it — AND (b) the backend
                // actually producing coherent memory checkpoints (FC
                // with dirty tracking). The latter keeps a split-mode
                // VZ / Process host (which the Tilt local setup runs)
                // from ever engaging the chain or the periodic driver
                // below — VZ has no guest-memory snapshot, so a
                // checkpoint there is just a wasteful VM pause.
                let checkpoints_supported = self.sandbox.supports_diff_checkpoints();
                if self.chunk_store.is_some() && checkpoints_supported {
                    p = p.with_checkpoint_dir(self.cfg.work_dir.join("checkpoints"));
                }
                Arc::new(p)
            };
            // ADR 0028 Fix A: the periodic checkpoint driver. No-ops
            // when ENGRAM_CHECKPOINT_INTERVAL_SECS=0, the backend has
            // no checkpoint dir, or the backend can't do diff
            // checkpoints (VZ / Process — never pauses their VMs).
            let _checkpoint_driver = if self.sandbox.supports_diff_checkpoints() {
                checkpoint::spawn_checkpoint_driver(
                    pooled.clone(),
                    checkpoint::CheckpointConfig::from_env(),
                )
            } else {
                None
            };
            // ADR 0013: every harness event POSTs to the coord via
            // HTTP. Any coord pod can serve the POST (the
            // `state.emit` path on the receiving pod handles
            // session_events persistence + SSE fan-out via
            // pg_listener). Per-event POSTs are slightly chattier
            // than the old WS-frame approach but eliminate the
            // pod-pinning required for a long-lived stream and the
            // back-to-back duplicate `harness_idle` de-dup at the
            // coord side already protects against the few extra
            // events that might land out-of-order across pods.
            let coord_client_for_events = coord_client::CoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            let event_sink = crate::harness::event_sink_to(move |session_id, sandbox_id, ev| {
                let cc = coord_client_for_events.clone();
                async move {
                    let req = coord_client::HarnessEventRequest {
                        sandbox_id,
                        event: ev,
                        at: chrono::Utc::now(),
                    };
                    if let Err(e) = cc.harness_event(session_id, &req).await {
                        tracing::debug!(
                            %session_id, %sandbox_id, error = %e,
                            "forward harness event to coord failed",
                        );
                    }
                }
            });
            let harness_hub = std::sync::Arc::new(crate::harness::HarnessHub::new(event_sink));
            // Plumb the hub into the FC/VZ backend's vsock-accept
            // sink so inbound harness connections land on the local
            // hub's adapter loop. Without this the FC backend drops
            // every dial with "no sink registered" — the bug Phase 2
            // closes.
            let sink_hub = harness_hub.clone();
            // ADR 0037: FC/VZ hand us the connection's sandbox id, so key
            // the harness connection on it directly (a warm-restored harness
            // re-attaches under its baked sentinel session id, which no
            // session_to_sandbox entry maps). `None` expected-session = the
            // per-sandbox UDS is the identity, so accept any session id.
            let sink: engram_core::traits::HarnessSink =
                std::sync::Arc::new(move |sandbox_id, stream| {
                    sink_hub.accept_connection(sandbox_id, None, stream)
                });
            pooled.set_harness_sink(sink);
            // ADR 0037: give the backend the hub so `build_base_snapshot`
            // can wait for a warm-capture harness to reach warm+idle.
            pooled.set_warm_capture_hub(harness_hub.clone());

            // ADR 0023 split-mode forge forwarding. The forge sink can't
            // live on the host (it needs the coord's GitForge + broker
            // map), so — exactly like the harness event forwarding above
            // — the host proxies each in-guest forge dial to the coord
            // over HTTP: read the `ForgeRequest` off the vsock stream,
            // POST it to `/api/hosts/forge`, write the `ForgeResponse`
            // back. Without this the FC forge accept loop has no sink and
            // drops every dial (guest sees "Broken pipe").
            let coord_client_for_forge = coord_client::CoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            let forge_sink: engram_core::traits::ForgeSink = std::sync::Arc::new(
                move |mut stream| {
                    let cc = coord_client_for_forge.clone();
                    tokio::spawn(async move {
                        let req: engram_harness_proto::ForgeRequest =
                            match engram_harness_proto::read_msg(&mut stream).await {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::debug!(error = %e, "forge: malformed request from guest");
                                    return;
                                }
                            };
                        let resp = match cc.forge(&req).await {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!(error = %e, "forge: forward to coord failed");
                                engram_harness_proto::ForgeResponse::Error {
                                    message: format!("forge forward to coord failed: {e}"),
                                }
                            }
                        };
                        if let Err(e) = engram_harness_proto::write_msg(&mut stream, &resp).await {
                            tracing::debug!(error = %e, "forge: response write to guest failed");
                        }
                    });
                },
            );
            pooled.set_forge_sink(forge_sink);

            // ADR 0026 split-mode artifact forwarding. Like forge above,
            // the upload sink can't live on the host (needs the coord's
            // BlobStorage + broker map). But unlike forge it must NOT
            // buffer — a multi-hundred-MB video would OOM the host. So the
            // host is a pure byte relay: read the `UploadRequest` header
            // off the vsock, then stream the raw body straight into a
            // streaming POST to `/api/hosts/upload`, and write the coord's
            // `UploadResponse` back to the guest.
            let coord_client_for_upload = coord_client::CoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );
            let upload_sink: engram_core::traits::UploadSink = std::sync::Arc::new(move |stream| {
                let cc = coord_client_for_upload.clone();
                tokio::spawn(async move {
                    // Split so we can stream the body off the read half
                    // (handed to reqwest) while keeping the write half
                    // to reply on.
                    let (mut read_half, mut write_half) = tokio::io::split(stream);
                    let header: engram_harness_proto::UploadRequest =
                        match engram_harness_proto::read_msg(&mut read_half).await {
                            Ok(h) => h,
                            Err(e) => {
                                tracing::debug!(error = %e, "upload: malformed header from guest");
                                return;
                            }
                        };
                    let engram_harness_proto::UploadOp::ShareFile { size_bytes, .. } = &header.op;
                    // Cap the body read at the declared size so the
                    // guest can't over-feed the relay; the coord also
                    // enforces MAX_ARTIFACT_BYTES while draining.
                    let body = tokio::io::AsyncReadExt::take(read_half, *size_bytes);
                    let resp = match cc.upload_artifact(&header, body).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "upload: forward to coord failed");
                            engram_harness_proto::UploadResponse::Error {
                                message: format!("upload forward to coord failed: {e}"),
                            }
                        }
                    };
                    if let Err(e) = engram_harness_proto::write_msg(&mut write_half, &resp).await {
                        tracing::debug!(error = %e, "upload: response write to guest failed");
                    }
                });
            });
            pooled.set_upload_sink(upload_sink);
            // ADR 0015 M5: warm-pool retired. LocalHostClient is just
            // the cold-create dispatch wrapper now.
            let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
                crate::host_client::LocalHostClient::new(pooled.clone(), harness_hub.clone()),
            );
            // ADR 0009 §2: populate `running_sandboxes` from
            // `backend.list()` on each heartbeat tick. The coord
            // intersects this against expected-active sessions to
            // detect divergence (missing sandbox → flip session per
            // §3). On `list()` error, ship an empty list — the
            // 3-strike grace window (15s) absorbs transient errors
            // without flipping live sessions.
            // Seed capacity once at startup from /proc/meminfo. Used
            // by the scheduler's fit check — without this the coord
            // sees `total_mib=0` and rejects every session.
            // TODO: `used_mib` accounting. Plumbing record_start /
            // record_stop hooks into the SandboxBackend is a follow-up;
            // until then the host always looks "fully available",
            // which lets the scheduler place but doesn't prevent
            // oversubscription.
            let host_total_mib = crate::resource::read_total_memory_mib();
            tracing::info!(
                host_total_mib,
                "capacity reporting seeded from /proc/meminfo"
            );
            // ADR 0007: surface the reaper to the coord. The host's
            // gRPC server exposes ReapMaterializeDir; the coord's
            // POST /api/admin/reap-materialize-dir fans it out
            // across every connected host. Hosts without a
            // chunk_store wiring leave admin_handler = None and
            // the gRPC method returns Unimplemented.
            let admin_handler: Option<
                std::sync::Arc<dyn engram_protocol::admin::HostAdminHandler>,
            > = self.chunk_store.as_ref().map(|(_, dir)| {
                std::sync::Arc::new(admin_handler::MaterializeDirReaper::new(dir.clone()))
                    as std::sync::Arc<dyn engram_protocol::admin::HostAdminHandler>
            });

            // ADR 0013: per-process CoordClient for HTTP traffic
            // (register, heartbeat, registry-auth, harness-events,
            // idle-eviction).
            let coord_client = coord_client::CoordClient::new(
                coord_url.clone(),
                self.cfg.coordinator_token.clone(),
            );

            // ADR 0013: register over HTTP so the coord persists
            // our host_addr column. Mandatory — the coord-side
            // GrpcHostPool can't dispatch to us until host_addr
            // lands. Background loop with exponential backoff.
            if let Some(advertise_addr) = self.cfg.grpc_advertise_addr.clone() {
                // hostname: prefer the env-supplied value (set by
                // the deployment / systemd unit on production
                // hosts), fall back to a synthetic host-<id>
                // string. The coord uses this for human display
                // only; uniqueness comes from host_id.
                let hostname = std::env::var("HOSTNAME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("host-{host_id}"));
                let register_req = coord_client::RegisterRequest {
                    host_id,
                    hostname,
                    host_addr: advertise_addr.clone(),
                    agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    wire_version: engram_protocol::WIRE_VERSION,
                    cloud_metadata: None,
                };
                let cc = coord_client.clone();
                let pooled_for_rehydrate = pooled.clone();
                tokio::spawn(async move {
                    let mut backoff = std::time::Duration::from_millis(500);
                    let cap = std::time::Duration::from_secs(30);
                    loop {
                        match cc.register(&register_req).await {
                            Ok(resp) => {
                                tracing::info!(
                                    host_id = %register_req.host_id,
                                    host_addr = %register_req.host_addr,
                                    rehydrate_sandboxes = resp.rehydrate_sandboxes.len(),
                                    "registered with coord via /api/hosts/register",
                                );
                                // ADR 0016 Phase B commit 7: rehydrate
                                // any survivors PG knew about before
                                // our restart. Best-effort per row;
                                // one failure shouldn't block the
                                // rest. Linux-only NBD path; the
                                // PooledBackend method short-circuits
                                // on missing config.
                                #[cfg(target_os = "linux")]
                                {
                                    rehydrate_survivors(
                                        &pooled_for_rehydrate,
                                        &resp.rehydrate_sandboxes,
                                    )
                                    .await;
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let _ = pooled_for_rehydrate;
                                    if !resp.rehydrate_sandboxes.is_empty() {
                                        tracing::info!(
                                            count = resp.rehydrate_sandboxes.len(),
                                            "rehydrate list returned but host isn't Linux/NBD; ignoring",
                                        );
                                    }
                                }
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    host_id = %register_req.host_id,
                                    error = %e,
                                    "host registration failed; will retry",
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(cap);
                            }
                        }
                    }
                });
            }

            // ADR 0013: boot the gRPC HostService server. The
            // coord's GrpcHostPool dials this address (populated
            // via /api/hosts/register) to dispatch coord→host
            // RPCs. Required in production; mode=all leaves
            // `grpc_listen_addr=None`.
            let grpc_task = self.cfg.grpc_listen_addr.map(|addr| {
                let local_for_grpc = local_host.clone();
                let admin_for_grpc = admin_handler.clone();
                tokio::spawn(async move {
                    if let Err(e) = grpc_server::boot(addr, local_for_grpc, admin_for_grpc).await {
                        tracing::error!(addr = %addr, error = %e, "gRPC server terminated with error");
                    }
                })
            });

            // ADR 0015 M5: image-prefetch supervisor. Watches the
            // heartbeat-ack's `enabled_images` set and pulls the
            // chunked rootfs for any image not yet local on this
            // host. Updates the shared `ImageReadiness` which the
            // heartbeat builder reads to populate `ready_images`.
            // Spawned only when chunk_store + image_cache are wired
            // (production hosts; dev-process backend lacks both and
            // simply never reports ready).
            let readiness = image_prefetch::ImageReadiness::new();
            // ADR 0018 Phase B: per-sandbox NBD-loss health signal.
            // Empty in production until the probe task is wired in a
            // follow-up commit. Surfaced now so commit 5 (coord-side
            // trigger) has a wire field to consume and tests can
            // inject unhealthy ids via an admin endpoint (commit 7).
            let nbd_health = crate::heartbeat::NbdHealthMonitor::new();
            let enabled_images_tx = match (
                self.chunk_store.as_ref().map(|(cs, _)| cs.clone()),
                self.chunk_cache.clone(),
            ) {
                (Some(chunk_store), Some(chunk_cache)) => {
                    // ADR 0022 Option A: when base-create is on the File
                    // backend (ENGRAM_FC_BASE_RESTORE_MODE=file), have the
                    // supervisor materialize each enabled image's contiguous
                    // per-template base memfile at residency — at the SAME
                    // path a base session.create restore reads
                    // (`pooled.snapshot_path_for(base_snapshot_id)`), so they
                    // agree by construction. One coherent switch: the env
                    // that flips base-create to File also turns this on.
                    let base_memfile_dir: Option<image_prefetch::SnapshotDirResolver> = matches!(
                        engram_sandbox_firecracker::base_restore_mode_from_env(),
                        Some(engram_sandbox_firecracker::RestoreMode::File)
                    )
                    .then(|| {
                        let p = pooled.clone();
                        let resolver: image_prefetch::SnapshotDirResolver =
                            std::sync::Arc::new(move |id| p.snapshot_path_for(id));
                        resolver
                    });
                    let (tx, _handle) = image_prefetch::spawn_supervisor(
                        chunk_store,
                        chunk_cache,
                        readiness.clone(),
                        base_memfile_dir,
                    );
                    Some(tx)
                }
                _ => {
                    tracing::warn!(
                        "image_prefetch supervisor disabled: chunk_store / chunk_cache \
                         missing — host will never report ready_images and coord will 503 every session",
                    );
                    None
                }
            };

            // ADR 0035: bundle store + supervisor. The stamp is read once
            // (hosts are immutable; only a MIG roll changes it). The
            // supervisor consumes the ack's `live_bundles` pin set:
            // prefetch missing pinned generations, sweep unpinned ones.
            let bundle_dir = bundles::bundle_dir_from_env();
            let current_bundles = bundles::read_stamp(&bundle_dir).await;
            let live_bundles_tx = self.chunk_store.as_ref().map(|(cs, _)| {
                bundles::spawn_supervisor(
                    bundles::BundleStore::new(cs.blob_storage().clone(), bundle_dir.clone()),
                    current_bundles.clone(),
                )
            });

            // ADR 0013: HTTP heartbeat loop. Posts
            // {capacity, local_snapshots, running_sandboxes,
            // draining, ready_images} every `heartbeat_interval` to
            // any coord pod via the L4-LB. Coord pod that receives
            // it updates host_registry, runs ADR 0009 reconcile, and
            // persists to PG. Idempotent across pods. The ack
            // carries `enabled_images`, which the supervisor diffs
            // against the local ready set.
            let heartbeat_interval = self.cfg.heartbeat_interval;
            let coord_for_heartbeat = coord_client.clone();
            let pooled_for_heartbeat = pooled.clone();
            let host_addr_for_heartbeat = self.cfg.grpc_advertise_addr.clone();
            let readiness_for_heartbeat = readiness.clone();
            let nbd_health_for_heartbeat = nbd_health.clone();
            let heartbeat_task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(heartbeat_interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let running_sandboxes = match pooled_for_heartbeat.list().await {
                        Ok(ids) => ids,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "backend.list() failed; reporting empty running_sandboxes",
                            );
                            Vec::new()
                        }
                    };
                    let running_count = running_sandboxes.len() as u32;
                    // ADR 0022 Option A: sample summed guest PSS/RSS across
                    // this host's live FC sandboxes — the density signal
                    // (Σpss/Σrss → ~1.0 under UFFD private copies, < 1.0 as
                    // File-backend siblings share one base memfile). Set
                    // both gauges every tick so a drained host reports 0,
                    // never a stale value. Error-tolerant + non-blocking
                    // (telemetry must not gate the workload); absent on
                    // VZ/non-Linux (guest_memory_stats → None).
                    if let Some(mem) = pooled_for_heartbeat.guest_memory_stats().await {
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_PSS_BYTES)
                            .set(mem.pss_bytes as f64);
                        ::metrics::gauge!(crate::metrics::SANDBOX_GUEST_RSS_BYTES)
                            .set(mem.rss_bytes as f64);
                    }
                    // ADR 0028 Fix A: re-advertise every un-acked
                    // durable checkpoint record until a coord acks it
                    // into PG. Empty when checkpointing is disabled.
                    let checkpoint_records = match pooled_for_heartbeat.checkpoint_records_dir() {
                        Some(dir) => checkpoint::CheckpointRecord::load_all(&dir).await,
                        None => Vec::new(),
                    };
                    let checkpoints = checkpoint_records
                        .iter()
                        .map(|r| engram_protocol::heartbeat::CheckpointAdvert {
                            snapshot_id: r.snapshot_id,
                            session_id: r.session_id,
                            sandbox_id: r.sandbox_id,
                            image_version: r.image_version.clone(),
                            size_bytes: r.size_bytes,
                            disk_manifest: r.disk_manifest,
                            memory_manifest: r.memory_manifest,
                            aux_bundles: r.aux_bundles.clone(),
                            paused_at: r.paused_at,
                            captured_at: r.captured_at,
                        })
                        .collect();
                    let req = coord_client::HeartbeatRequest {
                        capacity: engram_protocol::heartbeat::HostCapacityReport {
                            total_mib: host_total_mib,
                            used_mib: 0,
                            running_sandboxes: running_count,
                        },
                        local_snapshots: Vec::new(),
                        running_sandboxes,
                        draining: false,
                        host_addr: host_addr_for_heartbeat.clone(),
                        ready_images: readiness_for_heartbeat.snapshot(),
                        nbd_unhealthy: nbd_health_for_heartbeat.snapshot(),
                        current_bundles: current_bundles.clone(),
                        checkpoints,
                    };
                    match coord_for_heartbeat.heartbeat(host_id, &req).await {
                        Ok(resp) => {
                            if let Some(tx) = enabled_images_tx.as_ref() {
                                // send_modify avoids notifying on
                                // no-op (same set as last tick).
                                tx.send_if_modified(|cur| {
                                    if *cur == resp.enabled_images {
                                        false
                                    } else {
                                        *cur = resp.enabled_images;
                                        true
                                    }
                                });
                            }
                            // ADR 0035 §5: hand the pin set to the
                            // bundle supervisor (same no-op dedup).
                            if let Some(tx) = live_bundles_tx.as_ref() {
                                tx.send_if_modified(|cur| {
                                    if *cur == resp.live_bundles {
                                        false
                                    } else {
                                        *cur = resp.live_bundles;
                                        true
                                    }
                                });
                            }
                            // ADR 0028 Fix A: the coord recorded these
                            // checkpoints into PG — drop the durable
                            // record files (the PG rows own the
                            // references now).
                            if !resp.acked_checkpoints.is_empty() {
                                if let Some(dir) = pooled_for_heartbeat.checkpoint_records_dir() {
                                    checkpoint::CheckpointRecord::delete_acked(
                                        &dir,
                                        &resp.acked_checkpoints,
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                host_id = %host_id,
                                error = %e,
                                "heartbeat POST failed; retrying next tick",
                            );
                        }
                    }
                }
            });

            // ADR 0011 follow-up #2: host owns idle-eviction
            // detection (its HarnessHub is authoritative for "last
            // harness activity"). Push candidates to coord via
            // HTTP; coord-side pipeline (`evict_idle_session`)
            // runs the snapshot+destroy+mark-Idle dance on whatever
            // pod receives the POST. Idempotent across pods.
            let eviction_hub = harness_hub.clone();
            let eviction_coord = coord_client.clone();
            let idle_soft_ttl = idle_evictor::idle_ttl_from_env();
            let idle_hard_ttl = idle_evictor::idle_hard_ttl_from_env();
            // ADR 0014 issue #4: disk-pressure floor. When free disk
            // on the work_dir falls below this, we pause pushing
            // idle-evict candidates — coord-side retry storms (every
            // one of which writes ~4 GiB of FC memory dump pre-fix)
            // can't fill the disk if we never push them. The
            // companion fixes from #1/#2 also stop the per-retry
            // leak; this is the defense-in-depth backstop for any
            // future leak class we haven't anticipated.
            let eviction_work_dir = self.cfg.work_dir.clone();
            let eviction_floor_bytes = idle_evictor::disk_floor_bytes_from_env();
            let eviction_task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(idle_evictor::DEFAULT_POLL_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let (allow, free) =
                        idle_evictor::disk_pressure_check(&eviction_work_dir, eviction_floor_bytes);
                    if let Some(b) = free {
                        ::metrics::gauge!(crate::metrics::HOST_DISK_FREE_BYTES).set(b as f64);
                    }
                    if !allow {
                        ::metrics::counter!(crate::metrics::IDLE_EVICT_DISK_PRESSURE_HOLDS_TOTAL)
                            .increment(1);
                        tracing::warn!(
                            host_id = %host_id,
                            free_bytes = ?free,
                            floor_bytes = eviction_floor_bytes,
                            "idle-evict paused: disk pressure (free < floor)",
                        );
                        continue;
                    }
                    // ADR 0016 §A.1.5a: sweep wedged in-flight markers
                    // before reading candidates. A spawned POST that
                    // got stuck (reqwest future hung past its own
                    // 120s timeout for some pathological reason)
                    // would otherwise permanently block re-eviction of
                    // its sandbox. 180s is 1.5× the per-request
                    // timeout — by that point the POST is unambiguously
                    // dead, even if the spawned task somehow hasn't
                    // returned.
                    let stale =
                        eviction_hub.sweep_stale_evictions(std::time::Duration::from_secs(180));
                    if !stale.is_empty() {
                        tracing::warn!(
                            host_id = %host_id,
                            count = stale.len(),
                            "swept stale eviction-inflight markers (>180s); spawned POSTs presumed wedged",
                        );
                    }
                    // `idle_sandboxes` now skips sandboxes whose prior
                    // POST is still in flight (ADR 0016 §A.1.5a).
                    let pairs = eviction_hub.idle_sandboxes(idle_soft_ttl, idle_hard_ttl);
                    if pairs.is_empty() {
                        continue;
                    }
                    let sandbox_ids: Vec<SandboxId> = pairs.iter().map(|(_, sb)| *sb).collect();
                    let candidates: Vec<coord_client::IdleCandidate> = pairs
                        .into_iter()
                        .map(|(session_id, sandbox_id)| coord_client::IdleCandidate {
                            session_id,
                            sandbox_id,
                            idle_since: None,
                        })
                        .collect();
                    // Mark BEFORE the spawn so the next tick (10s
                    // away) can't double-post. The clear runs in the
                    // spawned task's finally block — covers success,
                    // transport error, and timeout uniformly.
                    for sb in &sandbox_ids {
                        eviction_hub.mark_eviction_inflight(*sb);
                    }
                    // Fire-and-forget: don't block the tick loop on
                    // the POST. Since ADR 0034 the POST is a fast
                    // nomination (coord flips Active→Evicting and
                    // returns; its eviction scanner runs the
                    // pipeline), so the marker clears in seconds and
                    // re-nomination dedup comes from the coord side
                    // (non-Active candidate → accepted no-op). The
                    // marker + 180s stale sweep stay as the
                    // within-tick guard (ADR 0016 §A.1.5a).
                    //
                    // Shutdown caveat: `eviction_task.abort()` on
                    // process exit will not wait for these spawned
                    // children. Detached POSTs may be torn down mid-
                    // flight. Acceptable — the coord-side pipeline
                    // is idempotent (the registry guard at the top
                    // of `evict_idle_session` short-circuits on
                    // re-entry per A.1.5b).
                    let coord = eviction_coord.clone();
                    let hub = eviction_hub.clone();
                    let sandbox_ids_for_clear = sandbox_ids.clone();
                    tokio::spawn(async move {
                        let outcome = coord
                            .push_idle_eviction_candidates(host_id, candidates)
                            .await;
                        // Finally: clear in-flight markers regardless
                        // of outcome. A failed POST should NOT keep
                        // the sandbox blocked from a retry on the
                        // next tick — but the next tick will only
                        // happen after this completes, which is the
                        // whole point of the gate.
                        for sb in &sandbox_ids_for_clear {
                            hub.clear_eviction_inflight(*sb);
                        }
                        match outcome {
                            Ok(resp) => {
                                tracing::debug!(
                                    %host_id,
                                    accepted = resp.accepted,
                                    failed = resp.failed,
                                    "idle-eviction POST completed",
                                );
                            }
                            Err(e) => {
                                tracing::debug!(
                                    %host_id,
                                    error = %e,
                                    "idle-eviction POST failed; \
                                     will retry on next tick after clear",
                                );
                            }
                        }
                    });
                }
            });

            shutdown_signal().await;
            heartbeat_task.abort();
            eviction_task.abort();
            if let Some(t) = grpc_task {
                t.abort();
            }

            // ADR 0009 Phase 7: SIGTERM-checkpoint pipeline. Runs
            // only when `ENGRAM_GRACEFUL_SHUTDOWN=1` (opt-in for
            // now). On signal: drain → checkpoint every live
            // sandbox in parallel → update each sandbox.json's
            // `last_local_snapshot` so the Phase 8 reattach can
            // restore from local NVMe when pidfd-path-1 fails
            // (case C', graceful host reboot).
            let scfg = crate::shutdown::ShutdownConfig::from_env();
            let _ = crate::shutdown::run(&scfg, pooled.clone(), self.cfg.work_dir.clone()).await;

            // ADR 0028 Fix C (OSS half): the SIGTERM checkpoint above
            // wrote durable records via Fix A (it runs through the
            // PooledBackend). Steady-state heartbeats already
            // reconciled every PRIOR checkpoint into PG; fire ONE
            // final heartbeat carrying the just-written records so the
            // SIGTERM checkpoint itself reaches PG before the MIG can
            // delete this host. Best-effort + bounded: if the coord is
            // mid-roll, the worst case is the session warm-recovers to
            // the last periodic checkpoint (≤ one cadence interval)
            // instead of the SIGTERM instant — still memory-preserving.
            if let Some(dir) = pooled.checkpoint_records_dir() {
                let records = crate::checkpoint::CheckpointRecord::load_all(&dir).await;
                if !records.is_empty() {
                    let checkpoints = records
                        .iter()
                        .map(|r| engram_protocol::heartbeat::CheckpointAdvert {
                            snapshot_id: r.snapshot_id,
                            session_id: r.session_id,
                            sandbox_id: r.sandbox_id,
                            image_version: r.image_version.clone(),
                            size_bytes: r.size_bytes,
                            disk_manifest: r.disk_manifest,
                            memory_manifest: r.memory_manifest,
                            aux_bundles: r.aux_bundles.clone(),
                            paused_at: r.paused_at,
                            captured_at: r.captured_at,
                        })
                        .collect();
                    let req = coord_client::HeartbeatRequest {
                        capacity: engram_protocol::heartbeat::HostCapacityReport {
                            total_mib: host_total_mib,
                            used_mib: 0,
                            running_sandboxes: 0,
                        },
                        local_snapshots: Vec::new(),
                        running_sandboxes: Vec::new(),
                        draining: true,
                        host_addr: self.cfg.grpc_advertise_addr.clone(),
                        ready_images: Vec::new(),
                        nbd_unhealthy: Vec::new(),
                        current_bundles: Vec::new(),
                        checkpoints,
                    };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        coord_client.heartbeat(host_id, &req),
                    )
                    .await
                    {
                        Ok(Ok(resp)) => {
                            crate::checkpoint::CheckpointRecord::delete_acked(
                                &dir,
                                &resp.acked_checkpoints,
                            )
                            .await;
                            tracing::info!(
                                acked = resp.acked_checkpoints.len(),
                                "shutdown: final heartbeat reconciled SIGTERM checkpoints into PG",
                            );
                        }
                        Ok(Err(e)) => tracing::warn!(error = %e,
                            "shutdown: final checkpoint-flush heartbeat failed; \
                             session falls back to the last periodic checkpoint"),
                        Err(_) => {
                            tracing::warn!("shutdown: final checkpoint-flush heartbeat timed out")
                        }
                    }
                }
            }
        } else {
            tracing::info!("no coordinator_endpoint set; standalone dev mode (ctrl-c to exit)");
            shutdown_signal().await;
        }

        tracing::info!("host-agent shutting down");
        Ok(())
    }
}

/// ADR 0016 Phase B commit 7: rebuild chunked-disk tracking for
/// survivors PG already had bound to this host pre-restart. Called
/// from the host-registration success branch with the list coord
/// returned. Best-effort per row: a failure on one survivor logs
/// + continues; the host doesn't fail registration over it.
///
/// Each survivor gets:
/// 1. NBD slot + ChunkedDiskBackend rebuilt from the effective
///    disk manifest (newer of `live_disk_manifest_*` and the
///    latest recoverable snapshot — picked server-side by
///    `MetadataStore::list_active_sandboxes_on_host_with_disk_manifest`).
/// 2. NBD daemon spawned.
/// 3. `nbd_sandboxes` entry installed.
/// 4. FlushScheduler spawned with the (session_id, sandbox_id)
///    pair. Continuous flush resumes immediately.
/// 5. `session_bindings` pre-populated so the publisher's
///    sandbox→session lookup finds the binding on the first
///    post-rehydrate tick.
///
/// Survivors without a chunked disk manifest (legacy sessions, or
/// hosts where the publisher never landed a value) are skipped
/// with a debug log. They run "untracked" until the next eviction
/// snapshot.
#[cfg(target_os = "linux")]
async fn rehydrate_survivors(
    pooled: &Arc<pooled_backend::PooledBackend>,
    survivors: &[coord_client::RehydrateSandboxRef],
) {
    if survivors.is_empty() {
        return;
    }
    let mut rehydrated = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for entry in survivors {
        let (Some(mid), Some(ver)) = (entry.disk_manifest_id, entry.disk_manifest_version) else {
            tracing::debug!(
                session_id = %entry.session_id,
                sandbox_id = %entry.sandbox_id,
                "rehydrate skipped: no disk manifest on the survivor row",
            );
            skipped += 1;
            continue;
        };
        let manifest_ref = engram_core::types::manifest::ManifestRef {
            manifest_id: mid,
            version: ver,
        };
        match pooled
            .rehydrate_sandbox(entry.session_id, entry.sandbox_id, manifest_ref)
            .await
        {
            Ok(true) => rehydrated += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::warn!(
                    session_id = %entry.session_id,
                    sandbox_id = %entry.sandbox_id,
                    manifest = %manifest_ref,
                    error = %e,
                    "rehydrate failed for survivor sandbox; continuing with the rest",
                );
                failed += 1;
            }
        }
    }
    tracing::info!(
        total = survivors.len(),
        rehydrated,
        skipped,
        failed,
        "Phase B rehydration pass complete",
    );
}

/// Await either SIGINT (ctrl-c) or SIGTERM (Kubernetes shutdown).
/// On non-unix platforms, falls back to ctrl-c only.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler; ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("host-agent received ctrl-c");
            }
            _ = term.recv() => {
                tracing::info!("host-agent received SIGTERM");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Debug)]
pub enum HostAgentError {
    Config(String),
    Io(std::io::Error),
    Backend(engram_core::BackendError),
}

impl std::fmt::Display for HostAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "host-agent config error: {msg}"),
            Self::Io(e) => write!(f, "host-agent io error: {e}"),
            Self::Backend(e) => write!(f, "host-agent backend error: {e}"),
        }
    }
}

impl std::error::Error for HostAgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Backend(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HostAgentError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
