//! Operator-owned files configure the dedicated listener and independent zones.
use super::{
    control::{self, ControlTls},
    model::Registry,
    worker::{WorkerClient, WorkerTransport},
    McpAuthority, McpError, SystemSecurityEntropy,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperatorConfig {
    registry: Registry,
    listen_addr: std::net::SocketAddr,
    server_certificate_file: PathBuf,
    server_private_key_file: PathBuf,
    orchestrator_ca_file: PathBuf,
    orchestrator_fingerprints: Vec<String>,
    workers: BTreeMap<String, WorkerFiles>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerFiles {
    url: String,
    server_ca_file: PathBuf,
    client_identity_file: PathBuf,
}
async fn bounded_file(path: &Path) -> super::Result<Vec<u8>> {
    if tokio::fs::metadata(path)
        .await
        .map_err(|_| McpError::Invalid)?
        .len()
        > 128 * 1024
    {
        return Err(McpError::Invalid);
    }
    let bytes = tokio::fs::read(path).await.map_err(|_| McpError::Invalid)?;
    if bytes.len() > 128 * 1024 {
        return Err(McpError::Invalid);
    }
    Ok(bytes)
}
pub async fn start(
    path: Option<&Path>,
    manager: Arc<crate::oauth::OAuthManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> super::Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(path) = path else { return Ok(None) };
    let config: OperatorConfig =
        serde_json::from_slice(&bounded_file(path).await?).map_err(|_| McpError::Invalid)?;
    if !config.registry.enabled {
        return Ok(None);
    }
    let mut workers = BTreeMap::new();
    for (id, files) in config.workers {
        let client = WorkerClient::new(
            &files.url,
            &bounded_file(&files.server_ca_file).await?,
            &bounded_file(&files.client_identity_file).await?,
        )?;
        workers.insert(id, Arc::new(client) as Arc<dyn WorkerTransport>);
    }
    let authority = Arc::new(McpAuthority::new(
        manager,
        Arc::new(SystemSecurityEntropy),
        config.registry,
        workers,
    )?);
    let tls = ControlTls {
        certificate: bounded_file(&config.server_certificate_file).await?,
        private_key: bounded_file(&config.server_private_key_file).await?,
        client_ca: bounded_file(&config.orchestrator_ca_file).await?,
        orchestrator_fingerprints: config.orchestrator_fingerprints,
    };
    let router = control::server(authority, tls)?;
    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(|_| McpError::Unavailable)?;
    Ok(Some(tokio::spawn(async move {
        let stop = async move {
            while !*shutdown.borrow_and_update() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        };
        if router
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener),
                stop,
            )
            .await
            .is_err()
        {
            tracing::error!("MCP control listener stopped");
        }
    })))
}
