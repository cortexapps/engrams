//! Operator-only publication service and durable readiness driver.
use engram_core::{
    traits::MetadataStore,
    types::{managed_boot::ManagedBootHostState, managed_deny::*, managed_operator::*},
};
use engram_protocol::mcp as wire;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tonic::{Request, Response, Status};
use wire::operator_publication_service_server::{
    OperatorPublicationService, OperatorPublicationServiceServer,
};

pub struct OperatorTls {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
    pub client_ca: Vec<u8>,
    pub fingerprints: Vec<String>,
}
#[derive(Clone)]
struct PublicationService {
    meta: Arc<dyn MetadataStore>,
    candidate: OperatorCandidate,
    pins: Arc<BTreeSet<String>>,
    admission: Arc<tokio::sync::Semaphore>,
}
fn valid_pin(s: &str) -> bool {
    s.len() == 95
        && s.split(':').all(|p| {
            p.len() == 2
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        })
}
pub fn server(
    meta: Arc<dyn MetadataStore>,
    candidate: OperatorCandidate,
    tls: OperatorTls,
    orchestrator_pins: &[String],
) -> Result<tonic::transport::server::Router, super::McpError> {
    candidate.identity().map_err(|_| super::McpError::Invalid)?;
    if tls.fingerprints.is_empty()
        || tls.fingerprints.len() > 8
        || tls
            .fingerprints
            .iter()
            .any(|p| !valid_pin(p) || orchestrator_pins.contains(p))
    {
        return Err(super::McpError::Invalid);
    }
    engram_tls::install_provider();
    let service = PublicationService {
        meta,
        candidate,
        pins: Arc::new(tls.fingerprints.into_iter().collect()),
        admission: Arc::new(tokio::sync::Semaphore::new(4)),
    };
    Ok(tonic::transport::Server::builder()
        .tls_config(
            tonic::transport::ServerTlsConfig::new()
                .identity(tonic::transport::Identity::from_pem(
                    tls.certificate,
                    tls.private_key,
                ))
                .client_ca_root(tonic::transport::Certificate::from_pem(tls.client_ca)),
        )
        .map_err(|_| super::McpError::Invalid)?
        .concurrency_limit_per_connection(4)
        .max_concurrent_streams(4)
        .timeout(Duration::from_secs(10))
        .add_service(
            OperatorPublicationServiceServer::new(service)
                .max_decoding_message_size(1024)
                .max_encoding_message_size(3 * 1024 * 1024),
        ))
}
impl PublicationService {
    fn check<T>(
        &self,
        req: &Request<T>,
        protocol: u32,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        let certs = req
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("operator certificate required"))?;
        let cert = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("operator certificate required"))?;
        let pin = Sha256::digest(cert.as_ref())
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        if !self.pins.contains(&pin) {
            return Err(Status::permission_denied("operator certificate denied"));
        }
        if protocol != wire::PROTOCOL {
            return Err(Status::invalid_argument("unsupported operator protocol"));
        }
        self.admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("operator capacity exceeded"))
    }
}
fn publication_bytes(value: OperatorPublication) -> Result<Vec<u8>, Status> {
    serde_json::to_vec(&value).map_err(|_| Status::internal("publication encoding failed"))
}
#[tonic::async_trait]
impl OperatorPublicationService for PublicationService {
    async fn stage(
        &self,
        req: Request<wire::OperatorPublicationServiceStageRequest>,
    ) -> Result<Response<wire::OperatorPublicationServiceStageResponse>, Status> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let request = req.into_inner();
        if self
            .candidate
            .identity()
            .map_err(|_| Status::unavailable("configured authority unavailable"))?
            != request.configured_candidate_identity
        {
            return Err(Status::permission_denied("configured authority mismatch"));
        }
        Ok(Response::new(
            wire::OperatorPublicationServiceStageResponse {
                protocol: wire::PROTOCOL,
                publication_json: publication_bytes(
                    self.meta
                        .stage_operator_publication(
                            request.expected_generation,
                            self.candidate.clone(),
                        )
                        .await
                        .map_err(|_| Status::failed_precondition("publication changed"))?,
                )?,
            },
        ))
    }
    async fn read(
        &self,
        req: Request<wire::OperatorPublicationServiceReadRequest>,
    ) -> Result<Response<wire::OperatorPublicationServiceReadResponse>, Status> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        Ok(Response::new(
            wire::OperatorPublicationServiceReadResponse {
                protocol: wire::PROTOCOL,
                publication_json: publication_bytes(
                    self.meta
                        .read_operator_publication()
                        .await
                        .map_err(|_| Status::unavailable("publication unavailable"))?,
                )?,
            },
        ))
    }
}

/// Called only after its retention transaction commits. Host evidence is fresh,
/// authenticated by HostClient, and then compared with the same durable snapshot.
async fn fence(
    state: &crate::state::SharedState,
    g: &OperatorGeneration,
    p: &OperatorParticipant,
) -> Result<OperatorReadiness, crate::error::ApiError> {
    if p.dormant() {
        return Ok(OperatorReadiness::NeverAllocated);
    }
    // A live receipt has no durable host/proxy lifecycle authority. Policy-only
    // legacy rows remain blockers; they cannot authorize even a host mutation.
    let (request, _, prepared) = p.boot.as_ref().ok_or_else(conflict)?;
    let key = &request.key;
    let target = p.target()?.ok_or_else(conflict)?;
    let mut observed_prepared = None;
    match state.services.host.read_managed_boot(key.clone()).await? {
        ManagedBootHostState::Retired { receipt } => {
            return Ok(OperatorReadiness::Retired(receipt))
        }
        ManagedBootHostState::Captured { authority } => {
            return Ok(OperatorReadiness::Captured(Box::new(authority)))
        }
        ManagedBootHostState::Prepared { receipt } => {
            receipt.validate()?;
            if receipt.request != *request {
                return Err(conflict().into());
            }
            observed_prepared = Some(receipt);
        }
        ManagedBootHostState::Missing { .. } | ManagedBootHostState::Failed { .. } => {
            return Err(conflict().into())
        }
        _ => {}
    }
    let expected = state
        .services
        .host
        .read_managed_policy(target.clone())
        .await?;
    // Both a CLOSED bootstrap and a final live owner use the same exact CAS.
    // A newer generation retires a managed owner; a rejected call proves nothing.
    let context = ManagedPolicyContext {
        target: target.clone(),
        operator_generation: g.generation,
        operator_identity: g.identity.clone(),
        binding_hash: expected
            .as_ref()
            .and_then(|r| r.context.binding_hash.clone()),
    };
    let guest_ip = expected
        .as_ref()
        .and_then(|r| r.network_proof.as_ref().map(|n| n.guest_ip))
        .or_else(|| observed_prepared.as_ref().map(|r| r.guest_ip))
        .or_else(|| prepared.as_ref().map(|r| r.guest_ip))
        .ok_or_else(conflict)?;
    let egress = engram_core::types::egress::SessionEgressPolicy {
        session_id: p.session_id,
        sandbox_id: target.sandbox_id,
        guest_ip,
        network_allow_hosts: Vec::new(),
        network_allow_host_patterns: Vec::new(),
        allow_all: false,
        secrets: Vec::new(),
        injects: Vec::new(),
        observes: Vec::new(),
        guest_services: Vec::new(),
        tunnels: Vec::new(),
        secret_mode: engram_core::types::image::SecretMode::Broker,
        apps: Vec::new(),
        managed_deny: Some(p.policy.clone()),
    };
    let _ = state
        .services
        .host
        .apply_managed_policy(ManagedPolicyApply {
            expected,
            context,
            egress,
        })
        .await;
    match state.services.host.read_managed_boot(key.clone()).await? {
        ManagedBootHostState::Retired { receipt } => Ok(OperatorReadiness::Retired(receipt)),
        ManagedBootHostState::Captured { authority } => {
            Ok(OperatorReadiness::Captured(Box::new(authority)))
        }
        _ => Err(conflict().into()),
    }
}
/// One bounded page per durable pass. No transaction is held across host I/O.
/// The final store CAS rechecks the complete participant set, not this page alone.
pub async fn run_once(
    state: &crate::state::SharedState,
    configured: &OperatorCandidate,
    after: Option<engram_core::SessionId>,
) -> Result<Option<engram_core::SessionId>, crate::error::ApiError> {
    let publication = state.services.meta.read_operator_publication().await?;
    let Some(g) = publication.pending else {
        return Ok(None);
    };
    if &g.candidate != configured || g.identity != configured.identity()? {
        return Err(conflict().into());
    }
    let ids = state
        .services
        .meta
        .list_operator_participants(after, 64)
        .await?;
    for sid in &ids {
        let p = state
            .services
            .meta
            .retain_operator_participant(&g, *sid)
            .await?;
        match tokio::time::timeout(Duration::from_secs(5), fence(state, &g, &p)).await {
            Ok(Ok(proof)) => {
                state
                    .services
                    .meta
                    .record_operator_readiness(&g, &p, proof)
                    .await?;
            }
            _ => {
                tracing::debug!("operator publication participant remains fenced from admission");
            }
        }
    }
    if ids.len() == 64 {
        return Ok(ids.last().copied());
    }
    // A blocked/changed participant leaves the durable generation pending.
    match state.services.meta.publish_operator_generation(&g).await {
        Ok(_) => {}
        Err(engram_core::MetaError::Conflict(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(None)
}
pub async fn drive(
    state: crate::state::SharedState,
    candidate: OperatorCandidate,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut after = None;
    loop {
        tokio::select! {
            changed=shutdown.changed()=>{ if changed.is_err() || *shutdown.borrow() { break; } },
            _=tokio::time::sleep(Duration::from_secs(1))=>{
                match run_once(&state,&candidate,after).await { Ok(next)=>after=next,Err(_)=>{after=None;tracing::warn!("operator publication remains closed");} }
            }
        }
    }
}
