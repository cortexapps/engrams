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

use std::sync::Arc;

use engram_core::traits::{CloudBackend, SandboxBackend};

pub mod config;
pub mod dialer;
pub mod heartbeat;
pub mod pool;
pub mod resource;
pub mod snapshot;

pub use config::HostAgentConfig;

pub struct HostAgent {
    pub cfg: HostAgentConfig,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub cloud: Arc<dyn CloudBackend>,
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
        }
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
            // No real Pool wiring yet — for 3b the host-agent doesn't
            // run a warm pool of its own (the coordinator's pool was
            // historically owned in-process). The heartbeat reports
            // empty pools/snapshots; the scheduler falls through to
            // capacity-based ranking. Real pool reporting lands once
            // the Pool moves host-side in a follow-up.
            let dialer_cfg = dialer::DialerConfig {
                coordinator_url: coord_url,
                auth_token: self.cfg.coordinator_token.clone(),
                heartbeat_interval: self.cfg.heartbeat_interval,
                heartbeat_provider: None,
            };
            let backend = self.sandbox.clone();
            let dialer_task = tokio::spawn(async move {
                if let Err(e) = dialer::run_dialer(dialer_cfg, host_id, backend).await {
                    tracing::error!(error = %e, "dialer terminated with error");
                }
            });
            tokio::signal::ctrl_c().await.map_err(HostAgentError::Io)?;
            dialer_task.abort();
        } else {
            tracing::info!("no coordinator_endpoint set; standalone dev mode (ctrl-c to exit)");
            tokio::signal::ctrl_c().await.map_err(HostAgentError::Io)?;
        }

        tracing::info!("host-agent received ctrl-c, shutting down");
        Ok(())
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
