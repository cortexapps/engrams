//! Per-host daemon. Wraps a [`SandboxBackend`], maintains a warm pool of
//! microVMs per active repo, runs the snapshot manager and resource
//! governor, and heartbeats the coordinator with capacity + warm-pool +
//! local-snapshot state.
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

pub mod blob;
pub mod config;
pub mod dialer;
pub mod disk_pressure;
pub mod egress;
pub mod flush;
pub mod harness;
pub mod heartbeat;
pub mod image_cache;
pub mod pool;
pub mod pooled_backend;
pub mod resource;
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
        }
    }

    /// Attach a `ChunkCache`. Optional; layers on top of
    /// `with_chunk_store` to amortise chunk reads across manifests
    /// (canonical-base images, forks).
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
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

        if let Some(coord_url) = self.cfg.coordinator_endpoint.clone() {
            let host_id = engram_core::HostId::new();
            tracing::info!(
                host_id = %host_id,
                coordinator = %coord_url,
                "dialing coordinator",
            );
            // Wrap the underlying SandboxBackend in a PooledBackend so
            // `create()` opportunistically returns warm slots and the
            // heartbeat ships real `(ready, target)` counts to the
            // coordinator's scheduler. With this in place the
            // scheduler's warm-pool branch in pick_for_session
            // actually fires instead of falling through to capacity.
            let pooled = {
                let mut p = pooled_backend::PooledBackend::new(
                    self.sandbox.clone(),
                    self.cfg.warm_pool_size,
                );
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
                    p = p.with_image_cache(ic);
                }
                Arc::new(p)
            };
            let pooled_for_dialer: Arc<dyn engram_core::traits::SandboxBackend> = pooled.clone();
            let pooled_for_hb = pooled.clone();
            let provider: dialer::HeartbeatProvider = std::sync::Arc::new(move || {
                (
                    engram_protocol::HostCapacityReport::default(),
                    pooled_for_hb.snapshot_warm_pools(),
                    Vec::new(),
                    false,
                )
            });
            let dialer_cfg = dialer::DialerConfig {
                coordinator_url: coord_url,
                auth_token: self.cfg.coordinator_token.clone(),
                heartbeat_interval: self.cfg.heartbeat_interval,
                heartbeat_provider: Some(provider),
                auth_session_handle: self.auth_session_handle.clone(),
            };
            let dialer_task = tokio::spawn(async move {
                if let Err(e) = dialer::run_dialer(dialer_cfg, host_id, pooled_for_dialer).await {
                    tracing::error!(error = %e, "dialer terminated with error");
                }
            });
            shutdown_signal().await;
            dialer_task.abort();
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
