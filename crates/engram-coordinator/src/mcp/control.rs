//! Dedicated mTLS service. Never add this router to the app gRPC listener.
use super::{model as domain, oauth::Callback, worker, McpAuthority, McpError};
use engram_core::types::org_secret::{McpSecretReservation, McpSecretRole};
use engram_protocol::mcp as wire;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tonic::{Request, Response, Status};
use wire::credential_control_service_server::{
    CredentialControlService, CredentialControlServiceServer,
};

type RpcResult<T> = std::result::Result<Response<T>, Status>;
pub struct ControlTls {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
    pub client_ca: Vec<u8>,
    pub orchestrator_fingerprints: Vec<String>,
}
#[derive(Clone)]
pub(super) struct Control {
    pub(super) authority: Arc<McpAuthority>,
    pins: Arc<BTreeSet<String>>,
    admission: Arc<tokio::sync::Semaphore>,
}
pub fn server(
    authority: Arc<McpAuthority>,
    tls: ControlTls,
) -> super::Result<tonic::transport::server::Router> {
    server_with_policy(authority, tls, None)
}
pub fn server_with_policy(
    authority: Arc<McpAuthority>,
    tls: ControlTls,
    state: Option<crate::state::SharedState>,
) -> super::Result<tonic::transport::server::Router> {
    if tls.orchestrator_fingerprints.is_empty()
        || tls.orchestrator_fingerprints.len() > 8
        || tls.orchestrator_fingerprints.iter().any(|s| {
            s.len() != 95
                || s.split(':').any(|p| {
                    p.len() != 2
                        || !p
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
                })
        })
    {
        return Err(McpError::Invalid);
    }
    engram_tls::install_provider();
    let service = Control {
        authority,
        pins: Arc::new(tls.orchestrator_fingerprints.into_iter().collect()),
        admission: Arc::new(tokio::sync::Semaphore::new(32)),
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
        .map_err(|_| McpError::Invalid)?
        .concurrency_limit_per_connection(16)
        .timeout(Duration::from_secs(30))
        .max_concurrent_streams(16)
        .add_service(
            CredentialControlServiceServer::new(service.clone())
                .max_decoding_message_size(32768)
                .max_encoding_message_size(32768),
        )
        .add_service(
            wire::session_policy_service_server::SessionPolicyServiceServer::new(
                super::policy::PolicyControl {
                    control: service,
                    state,
                },
            )
            .max_decoding_message_size(4 * 1024 * 1024)
            .max_encoding_message_size(4 * 1024 * 1024),
        ))
}
impl Control {
    pub(super) fn check<T>(
        &self,
        req: &Request<T>,
        protocol: u32,
    ) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, Status> {
        let certs = req
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let cert = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let pin = Sha256::digest(cert.as_ref())
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        if !self.pins.contains(&pin) {
            return Err(Status::permission_denied("caller denied"));
        }
        if protocol != wire::PROTOCOL {
            return Err(Status::invalid_argument("unsupported control protocol"));
        }
        self.admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("control capacity exceeded"))
    }
}
fn error(e: McpError) -> Status {
    match e {
        McpError::NotFound => Status::not_found("credential not found"),
        McpError::Invalid => Status::invalid_argument("invalid credential request"),
        McpError::Denied => Status::permission_denied("credential request denied"),
        McpError::Stale => Status::failed_precondition("credential context changed"),
        _ => Status::unavailable("credential unavailable"),
    }
}
fn required<T>(value: Option<T>) -> std::result::Result<T, Status> {
    value.ok_or_else(|| Status::invalid_argument("required field missing"))
}
fn actor(value: &str) -> std::result::Result<(), Status> {
    if value.is_empty() || value.len() > 256 {
        Err(Status::invalid_argument("actor required"))
    } else {
        Ok(())
    }
}
fn destination(
    value: Option<wire::Destination>,
) -> std::result::Result<domain::Destination, Status> {
    let v = required(value)?;
    Ok(domain::Destination {
        zone_id: v.zone_id,
        endpoint: v.endpoint,
    })
}
fn change(
    value: Option<wire::ChangeContext>,
) -> std::result::Result<domain::ChangeContext, Status> {
    let v = required(value)?;
    Ok(domain::ChangeContext {
        connection_id: v.connection_id,
        config_revision: v.config_revision,
        previous_authorization_generation: v.previous_authorization_generation,
        resource: destination(v.resource)?,
    })
}
fn context(
    value: Option<wire::AuthorityContext>,
) -> std::result::Result<domain::AuthorityContext, Status> {
    let v = required(value)?;
    Ok(domain::AuthorityContext {
        connection_id: v.connection_id,
        config_revision: v.config_revision,
        authorization_generation: v.authorization_generation,
        credential_identity: v.credential_identity,
        resource: destination(v.resource)?,
        policy_digest: v.policy_digest,
        policy_revision: v.policy_revision,
    })
}
fn reference(
    value: wire::CredentialReference,
) -> std::result::Result<domain::CredentialReference, Status> {
    match wire::CredentialKind::try_from(value.kind)
        .map_err(|_| Status::invalid_argument("invalid credential kind"))?
    {
        wire::CredentialKind::ApiKey | wire::CredentialKind::ClientSecret
            if value.registration_id.is_empty()
                && !value.name.is_empty()
                && !value.connection_id.is_empty()
                && value.generation >= 1 =>
        {
            let role = if value.kind == wire::CredentialKind::ApiKey as i32 {
                McpSecretRole::ApiKey
            } else {
                McpSecretRole::ClientSecret
            };
            let reservation = McpSecretReservation {
                name: value.name,
                connection_id: value.connection_id,
                role,
                generation: value.generation,
            };
            Ok(if role == McpSecretRole::ApiKey {
                domain::CredentialReference::ApiKey { reservation }
            } else {
                domain::CredentialReference::ClientSecret { reservation }
            })
        }
        wire::CredentialKind::Oauth
            if !value.registration_id.is_empty()
                && value.name.is_empty()
                && value.connection_id.is_empty()
                && value.generation == 0 =>
        {
            Ok(domain::CredentialReference::OAuth {
                registration_id: value.registration_id,
            })
        }
        _ => Err(Status::invalid_argument("invalid credential reference")),
    }
}
fn context_wire(v: domain::AuthorityContext) -> wire::AuthorityContext {
    wire::AuthorityContext {
        connection_id: v.connection_id,
        config_revision: v.config_revision,
        authorization_generation: v.authorization_generation,
        credential_identity: v.credential_identity,
        resource: Some(wire::Destination {
            zone_id: v.resource.zone_id,
            endpoint: v.resource.endpoint,
        }),
        policy_digest: v.policy_digest,
        policy_revision: v.policy_revision,
    }
}
fn reference_wire(v: domain::CredentialReference) -> wire::CredentialReference {
    match v {
        domain::CredentialReference::ApiKey { reservation }
        | domain::CredentialReference::ClientSecret { reservation } => wire::CredentialReference {
            kind: if reservation.role == McpSecretRole::ApiKey {
                wire::CredentialKind::ApiKey
            } else {
                wire::CredentialKind::ClientSecret
            } as i32,
            name: reservation.name,
            connection_id: reservation.connection_id,
            generation: reservation.generation,
            registration_id: String::new(),
        },
        domain::CredentialReference::OAuth { registration_id } => wire::CredentialReference {
            kind: wire::CredentialKind::Oauth as i32,
            registration_id,
            ..Default::default()
        },
    }
}
fn status_wire(v: domain::CredentialStatus) -> wire::CredentialStatus {
    wire::CredentialStatus {
        protocol: wire::PROTOCOL,
        context: Some(context_wire(v.context)),
        reference: Some(reference_wire(v.reference)),
        version: v.version,
        status: v.status,
    }
}
fn status(v: wire::CredentialStatus) -> std::result::Result<domain::CredentialStatus, Status> {
    if v.protocol != wire::PROTOCOL {
        return Err(Status::invalid_argument("unsupported control protocol"));
    }
    Ok(domain::CredentialStatus {
        context: context(v.context)?,
        reference: reference(required(v.reference)?)?,
        version: v.version,
        status: v.status,
    })
}
#[tonic::async_trait]
impl CredentialControlService for Control {
    async fn put_secret(
        &self,
        req: Request<wire::PutSecretRequest>,
    ) -> RpcResult<wire::PutSecretResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        actor(&v.actor_id)?;
        let role = match wire::CredentialKind::try_from(v.kind) {
            Ok(wire::CredentialKind::ApiKey) => McpSecretRole::ApiKey,
            Ok(wire::CredentialKind::ClientSecret) => McpSecretRole::ClientSecret,
            _ => return Err(Status::invalid_argument("invalid secret kind")),
        };
        let expected = v.expected.map(reference).transpose()?;
        Ok(Response::new(wire::PutSecretResponse {
            status: Some(status_wire(
                self.authority
                    .put_secret(
                        &change(v.context)?,
                        role,
                        expected.as_ref(),
                        &v.header,
                        &v.value,
                    )
                    .await
                    .map_err(error)?,
            )),
        }))
    }
    async fn begin_authorization(
        &self,
        req: Request<wire::BeginAuthorizationRequest>,
    ) -> RpcResult<wire::BeginAuthorizationResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        actor(&v.actor_id)?;
        let secret = v.client_secret.map(reference).transpose()?;
        let b = self
            .authority
            .begin(
                &change(v.context)?,
                &v.registration_id,
                secret.as_ref(),
                v.expected_version,
                &v.actor_id,
                v.sharing_consent,
            )
            .await
            .map_err(error)?;
        Ok(Response::new(wire::BeginAuthorizationResponse {
            protocol: wire::PROTOCOL,
            flow_id: b.flow_id.to_string(),
            context: Some(context_wire(b.context)),
            authorization_url: b.authorization_url,
            expires_at: b.expires_at.timestamp_millis(),
        }))
    }
    async fn complete_authorization(
        &self,
        req: Request<wire::CompleteAuthorizationRequest>,
    ) -> RpcResult<wire::CompleteAuthorizationResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        actor(&v.actor_id)?;
        let callback = Callback {
            context: context(v.context)?,
            registration_id: v.registration_id,
            flow_id: v
                .flow_id
                .parse()
                .map_err(|_| Status::invalid_argument("invalid flow id"))?,
            state: v.state,
            redirect_uri: v.redirect_uri,
            issuer: v.issuer,
            code: v.code,
            actor_id: v.actor_id,
        };
        Ok(Response::new(wire::CompleteAuthorizationResponse {
            status: Some(status_wire(
                self.authority.callback(&callback).await.map_err(error)?,
            )),
        }))
    }
    async fn cancel_authorization(
        &self,
        req: Request<wire::CancelAuthorizationRequest>,
    ) -> RpcResult<wire::CancelAuthorizationResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        actor(&v.actor_id)?;
        let id = v
            .flow_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid flow id"))?;
        self.authority
            .cancel_authorization(id, &v.connection_id, &v.registration_id)
            .await
            .map_err(error)?;
        Ok(Response::new(wire::CancelAuthorizationResponse {
            protocol: wire::PROTOCOL,
            flow_id: v.flow_id,
            connection_id: v.connection_id,
            registration_id: v.registration_id,
        }))
    }
    async fn get_status(
        &self,
        req: Request<wire::GetStatusRequest>,
    ) -> RpcResult<wire::GetStatusResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        Ok(Response::new(wire::GetStatusResponse {
            status: Some(status_wire(
                self.authority
                    .status(&v.connection_id, &reference(required(v.reference)?)?)
                    .await
                    .map_err(error)?,
            )),
        }))
    }
    async fn revoke(&self, req: Request<wire::RevokeRequest>) -> RpcResult<wire::RevokeResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        actor(&v.actor_id)?;
        Ok(Response::new(wire::RevokeResponse {
            status: Some(status_wire(
                self.authority
                    .revoke(&status(required(v.expected)?)?)
                    .await
                    .map_err(error)?,
            )),
        }))
    }
    async fn lease(&self, req: Request<wire::LeaseRequest>) -> RpcResult<wire::LeaseResponse> {
        let _permit = self.check(&req, req.get_ref().protocol)?;
        let v = req.into_inner();
        let i = required(v.worker)?;
        let a = required(v.admission)?;
        let scope = match required(v.scope)? {
            wire::lease_request::Scope::Administration(s) => worker::WorkerScope::Administration {
                request_id: s.request_id,
                actor_id: s.actor_id,
            },
            wire::lease_request::Scope::Execution(s) => worker::WorkerScope::Execution {
                session_id: s.session_id,
                run_id: s.run_id,
                tool_call_id: s.tool_call_id,
                binding_hash: s.binding_hash,
            },
        };
        let lease = domain::ResourceLease {
            context: context(v.context)?,
            reference: reference(required(v.reference)?)?,
            worker: worker::WorkerIdentity {
                protocol: i.protocol,
                boot_id: i.boot_id,
                zone_id: i.zone_id,
                policy_digest: i.policy_digest,
                policy_revision: i.policy_revision,
            },
            intent_id: v.intent_id,
            request_digest: v.request_digest,
            operation: v.operation,
            expires_at: v.expires_at,
            operation_expires_at: v.operation_expires_at,
            admission: worker::WorkerLease {
                id: a.id,
                deployment_id: a.deployment_id,
                expires_at: a.expires_at,
            },
            scope,
        };
        let credential = self.authority.lease(&lease).await.map_err(error)?;
        Ok(Response::new(wire::LeaseResponse {
            protocol: wire::PROTOCOL,
            header: credential.header,
            value: credential.value,
            expires_at: lease.expires_at,
            operation_expires_at: lease.operation_expires_at,
            intent_id: lease.intent_id,
        }))
    }
}
