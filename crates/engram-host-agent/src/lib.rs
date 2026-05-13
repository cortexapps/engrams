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

use crate::image_cache::ImageCache;

pub mod admin_handler;
pub mod blob;
pub mod config;
pub mod dialer;
pub mod disk_daemon;
pub mod egress;
pub mod harness;
pub mod host_client;
pub use host_client::LocalHostClient;
pub mod heartbeat;
pub mod image_cache;
pub mod live_attach;
pub mod orphan_reap;
pub mod pooled_backend;
pub mod resource;
pub mod shutdown;
pub mod snapshot;
pub mod ws_auth;

pub use config::HostAgentConfig;

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
    /// ADR 0007: shared handle the dialer writes the live
    /// `HostSession` into when the WS comes up. The
    /// [`ws_auth::WsAuthResolver`] reads it for each OCI auth
    /// lookup so credentials can travel back over the existing
    /// WS connection. `None` skips the wiring entirely.
    pub auth_session_handle: Option<ws_auth::SessionHandle>,
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
            auth_session_handle: None,
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

    /// Wire the session handle the dialer will populate so the
    /// `WsAuthResolver` can issue OCI auth RPCs over the live WS.
    /// Pair with the resolver returned by
    /// [`ws_auth::WsAuthResolver::new`].
    pub fn with_auth_session_handle(mut self, handle: ws_auth::SessionHandle) -> Self {
        self.auth_session_handle = Some(handle);
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
                Arc::new(p)
            };
            // Construct a local HarnessHub whose EventSink ships
            // `NotifyKind::HarnessEvent` over the WS dialer. The
            // dialer populates `harness_session_handle` on every
            // connect and clears it on disconnect, so a brief gap
            // during reconnect just drops a handful of events
            // (the host's hub still drives idle eviction locally).
            let harness_session_handle: crate::ws_auth::SessionHandle =
                Arc::new(tokio::sync::RwLock::new(None));
            let sink_handle = harness_session_handle.clone();
            let event_sink = crate::harness::event_sink_to(move |session_id, sandbox_id, ev| {
                let handle = sink_handle.clone();
                async move {
                    let guard = handle.read().await;
                    let Some(session) = guard.clone() else {
                        tracing::trace!(
                            %session_id, %sandbox_id,
                            "no live coord session; dropping harness event",
                        );
                        return;
                    };
                    drop(guard);
                    let frame = engram_protocol::wire::NotifyKind::HarnessEvent {
                        session_id,
                        sandbox_id,
                        event: ev,
                        at: chrono::Utc::now(),
                    };
                    if let Err(e) = session.notify(frame).await {
                        tracing::debug!(
                            %session_id, %sandbox_id, error = %e,
                            "forward harness event over WS failed",
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
            let sink: engram_core::traits::HarnessSink =
                std::sync::Arc::new(move |stream| sink_hub.accept_via_session_lookup(stream));
            pooled.set_harness_sink(sink);
            let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
                crate::host_client::LocalHostClient::new(pooled.clone(), harness_hub),
            );
            let host_for_dialer = local_host.clone();
            // ADR 0009 §2: populate `running_sandboxes` from
            // `backend.list()` on each heartbeat tick. The coord
            // intersects this against expected-active sessions to
            // detect divergence (missing sandbox → flip session per
            // §3). On `list()` error, ship an empty list — the
            // 3-strike grace window (15s) absorbs transient errors
            // without flipping live sessions.
            let pooled_for_provider = pooled.clone();
            let provider: dialer::HeartbeatProvider = std::sync::Arc::new(move || {
                let backend = pooled_for_provider.clone();
                Box::pin(async move {
                    let running_sandboxes = match backend.list().await {
                        Ok(ids) => ids,
                        Err(e) => {
                            tracing::warn!(error = %e, "backend.list() failed; reporting empty running_sandboxes");
                            Vec::new()
                        }
                    };
                    dialer::HeartbeatPayload {
                        capacity: engram_protocol::HostCapacityReport::default(),
                        local_snapshots: Vec::new(),
                        running_sandboxes,
                        draining: false,
                    }
                })
            });
            // ADR 0007: surface the reaper to the coord. The coord's
            // POST /api/admin/reap-materialize-dir fans this out
            // across every connected host in --mode=coordinator;
            // without it, that admin endpoint can't reach this
            // host. We hand it `materialize_dir` from
            // chunk_store; hosts without a chunk_store wiring
            // leave admin_handler = None (RPC then returns a clean
            // "unsupported" error, the coord-side aggregator
            // tolerates per-host failures).
            let admin_handler: Option<
                std::sync::Arc<dyn engram_protocol::server::HostAdminHandler>,
            > = self.chunk_store.as_ref().map(|(_, dir)| {
                std::sync::Arc::new(admin_handler::MaterializeDirReaper::new(dir.clone()))
                    as std::sync::Arc<dyn engram_protocol::server::HostAdminHandler>
            });
            let dialer_cfg = dialer::DialerConfig {
                coordinator_url: coord_url,
                auth_token: self.cfg.coordinator_token.clone(),
                heartbeat_interval: self.cfg.heartbeat_interval,
                heartbeat_provider: Some(provider),
                auth_session_handle: self.auth_session_handle.clone(),
                harness_session_handle: Some(harness_session_handle),
                admin_handler,
            };
            let dialer_task = tokio::spawn(async move {
                if let Err(e) = dialer::run_dialer(dialer_cfg, host_id, host_for_dialer).await {
                    tracing::error!(error = %e, "dialer terminated with error");
                }
            });
            shutdown_signal().await;
            dialer_task.abort();

            // ADR 0009 Phase 7: SIGTERM-checkpoint pipeline. Runs
            // only when `ENGRAM_GRACEFUL_SHUTDOWN=1` (opt-in for
            // now). On signal: drain → checkpoint every live
            // sandbox in parallel → update each sandbox.json's
            // `last_local_snapshot` so the Phase 8 reattach can
            // restore from local NVMe when pidfd-path-1 fails
            // (case C', graceful host reboot).
            let scfg = crate::shutdown::ShutdownConfig::from_env();
            let _ = crate::shutdown::run(&scfg, pooled.clone(), self.cfg.work_dir.clone()).await;
        } else {
            tracing::info!("no coordinator_endpoint set; standalone dev mode (ctrl-c to exit)");
            shutdown_signal().await;
        }

        tracing::info!("host-agent shutting down");
        Ok(())
    }
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
