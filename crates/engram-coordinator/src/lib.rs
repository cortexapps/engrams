//! HTTP API + scheduler for Engram.
//!
//! Phase 1 surface: `POST /sessions`, `POST /sessions/:id/exec`,
//! `DELETE /sessions/:id`, plus snapshot/restore stubs and a healthz.
//! Multiple coordinator replicas are supported by virtue of all state
//! living in Postgres; this binary itself is stateless.

use std::sync::Arc;

use engram_core::traits::{BlobStorage, CloudBackend, MetadataStore, SandboxBackend, SecretStore};

pub mod api;
pub mod config;
pub mod error;
pub mod image_registry;
pub mod scheduler;
pub mod state;

pub use config::CoordinatorConfig;
pub use error::ApiError;
pub use state::AppState;

/// Container for the wired-up dependencies. Built once at startup;
/// passed by `Arc<AppState>` into the axum router.
pub struct Services {
    pub meta: Arc<dyn MetadataStore>,
    pub blob: Arc<dyn BlobStorage>,
    pub cloud: Arc<dyn CloudBackend>,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub secrets: Arc<dyn SecretStore>,
    pub images: image_registry::ImageRegistry,
}

/// Bootstrap the axum server. Returns once the bind future yields.
pub async fn run(cfg: CoordinatorConfig, services: Services) -> Result<(), CoordinatorError> {
    let state = Arc::new(AppState::new(cfg.clone(), services));
    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr.as_str())
        .await
        .map_err(CoordinatorError::Io)?;
    tracing::info!(addr = %cfg.bind_addr, "coordinator listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(CoordinatorError::Io)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("coordinator received ctrl-c, shutting down");
}

#[derive(Debug)]
pub enum CoordinatorError {
    Config(String),
    Io(std::io::Error),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "coordinator config error: {msg}"),
            Self::Io(e) => write!(f, "coordinator io error: {e}"),
        }
    }
}

impl std::error::Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
