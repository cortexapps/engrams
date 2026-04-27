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
    pub async fn run(self) -> Result<(), HostAgentError> {
        tracing::info!(?self.cfg.work_dir, "host-agent starting");
        let _ = self.cloud.host_metadata().await;
        // TODO(phase-1): kick off pool warmer, snapshot manager,
        // heartbeat loop on tokio::spawn handles. For now we wait for
        // ctrl-c so the binary stays alive in dev.
        tokio::signal::ctrl_c().await.map_err(HostAgentError::Io)?;
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
