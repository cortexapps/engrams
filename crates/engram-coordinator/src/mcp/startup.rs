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
    #[serde(default)]
    publication: Option<PublicationFiles>,
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
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> super::Result<Option<tokio::task::JoinHandle<()>>> {
    start_with_policy(path, manager, shutdown, None).await
}
pub async fn start_with_policy(
    path: Option<&Path>,
    manager: Arc<crate::oauth::OAuthManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    state: Option<crate::state::SharedState>,
) -> super::Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(path) = path else { return Ok(None) };
    let config: OperatorConfig =
        serde_json::from_slice(&bounded_file(path).await?).map_err(|_| McpError::Invalid)?;
    if !config.registry.enabled {
        return start_publication(
            config.publication,
            &config.orchestrator_fingerprints,
            state,
            shutdown,
        )
        .await;
    }
    let operator_pins = config.orchestrator_fingerprints.clone();
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
    let router = control::server_with_policy(authority, tls, state.clone())?;
    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(|_| McpError::Unavailable)?;
    let publication =
        start_publication(config.publication, &operator_pins, state, shutdown.clone()).await?;
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
        if let Some(publication) = publication {
            publication.abort();
            let _ = publication.await;
        }
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationFiles {
    listen_addr: std::net::SocketAddr,
    server_certificate_file: PathBuf,
    server_private_key_file: PathBuf,
    operator_ca_file: PathBuf,
    operator_fingerprints: Vec<String>,
    candidate_file: PathBuf,
    ca_files: BTreeMap<String, PathBuf>,
}
async fn start_publication(
    files: Option<PublicationFiles>,
    orchestrator_pins: &[String],
    state: Option<crate::state::SharedState>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> super::Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(files) = files else { return Ok(None) };
    let state = state.ok_or(McpError::Unavailable)?;
    let candidate: engram_core::types::managed_operator::OperatorCandidate =
        serde_json::from_slice(&bounded_file(&files.candidate_file).await?)
            .map_err(|_| McpError::Invalid)?;
    candidate.identity().map_err(|_| McpError::Invalid)?;
    if files.ca_files.len() != candidate.ca_content_digests.len() {
        return Err(McpError::Invalid);
    }
    for (id, path) in files.ca_files {
        let actual = engram_core::types::managed_create::digest(&bounded_file(&path).await?);
        if candidate.ca_content_digests.get(&id) != Some(&actual) {
            return Err(McpError::Invalid);
        }
    }
    let tls = super::operator::OperatorTls {
        certificate: bounded_file(&files.server_certificate_file).await?,
        private_key: bounded_file(&files.server_private_key_file).await?,
        client_ca: bounded_file(&files.operator_ca_file).await?,
        fingerprints: files.operator_fingerprints,
    };
    let router = super::operator::server(
        state.services.meta.clone(),
        candidate.clone(),
        tls,
        orchestrator_pins,
    )?;
    let listener = tokio::net::TcpListener::bind(files.listen_addr)
        .await
        .map_err(|_| McpError::Unavailable)?;
    Ok(Some(tokio::spawn(async move {
        let mut stop = shutdown.clone();
        let serving = router.serve_with_incoming_shutdown(
            tonic::transport::server::TcpIncoming::from(listener),
            async move {
                while !*stop.borrow_and_update() {
                    if stop.changed().await.is_err() {
                        break;
                    }
                }
            },
        );
        // If either producer exits, drop the other. No detached scanner survives it.
        tokio::select! {
            result=serving=>{if result.is_err(){tracing::error!("operator listener stopped");}},
            _=super::operator::drive(state,candidate,shutdown)=>{},
        }
    })))
}
