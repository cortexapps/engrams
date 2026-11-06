//! The coordinator app-gRPC calls the drain makes. It runs against the
//! coordinator that still serves, so it uses only RPCs the previous release
//! has.

use std::time::Duration;

use engram_core::HostId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;
use tonic::codegen::InterceptedService;
use tonic::transport::{Channel, Endpoint};

use crate::DrainError;

/// Sends `Authorization: Bearer <token>`. `None` omits the header, for a
/// coordinator that accepts no tokens.
#[derive(Clone)]
pub struct Bearer {
    token: Option<String>,
}

impl Bearer {
    pub fn new(token: Option<String>) -> Self {
        Self { token }
    }
}

impl tonic::service::Interceptor for Bearer {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.token {
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| tonic::Status::invalid_argument("bearer token is not ASCII"))?,
            );
        }
        Ok(req)
    }
}

/// A lazily connected channel to the coordinator's app-gRPC address. An
/// address without a scheme gets `http://`.
pub fn channel(endpoint: &str, call_timeout: Duration) -> Channel {
    let endpoint = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    Endpoint::from_shared(endpoint.clone())
        .unwrap_or_else(|e| panic!("invalid coordinator gRPC endpoint {endpoint:?}: {e}"))
        .connect_timeout(Duration::from_secs(5))
        .timeout(call_timeout)
        .connect_lazy()
}

pub fn parse_host_id(id: &str) -> Result<HostId, DrainError> {
    id.parse()
        .map_err(|_| DrainError::Invalid(format!("coordinator returned malformed host id {id:?}")))
}

/// A host as the drain reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeHost {
    pub id: HostId,
    /// `ready` or `draining`: the host may still run VMs.
    pub live: bool,
    pub cordoned: bool,
    pub running_sandboxes: u32,
    pub reattach_generation: u32,
}

/// A session as the drain reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeSession {
    pub id: String,
    /// The coordinator's state name (`engram_core::SessionState::as_str`).
    pub state: String,
    pub host_id: Option<HostId>,
}

pub struct Coordinator {
    fleet: FleetServiceClient<InterceptedService<Channel, Bearer>>,
    sessions: SessionServiceClient<InterceptedService<Channel, Bearer>>,
}

impl Coordinator {
    /// Fleet calls time out after 10 s. `session_timeout` bounds each
    /// session call; an eviction captures the guest, so it must cover the
    /// largest guest's capture.
    pub fn new(endpoint: &str, token: Option<String>, session_timeout: Duration) -> Self {
        Self {
            fleet: FleetServiceClient::with_interceptor(
                channel(endpoint, Duration::from_secs(10)),
                Bearer::new(token.clone()),
            ),
            sessions: SessionServiceClient::with_interceptor(
                channel(endpoint, session_timeout),
                Bearer::new(token),
            ),
        }
    }

    pub async fn list_hosts(&self) -> Result<Vec<UpgradeHost>, DrainError> {
        let resp = self
            .fleet
            .clone()
            .list_hosts(app::ListHostsRequest {})
            .await
            .map_err(|status| DrainError::rpc("list_hosts", status))?;
        resp.into_inner()
            .hosts
            .into_iter()
            .map(|v| {
                Ok(UpgradeHost {
                    id: parse_host_id(&v.id)?,
                    live: matches!(v.status.as_str(), "ready" | "draining"),
                    cordoned: v.cordoned,
                    running_sandboxes: v.running_sandboxes,
                    reattach_generation: v.reattach_generation,
                })
            })
            .collect()
    }

    pub async fn cordon(&self, host: HostId, owner: &str) -> Result<(), DrainError> {
        self.fleet
            .clone()
            .cordon_host(app::CordonHostRequest {
                owner: owner.into(),
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| DrainError::rpc("cordon", status))?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<UpgradeSession>, DrainError> {
        let resp = self
            .sessions
            .clone()
            .list_sessions(app::ListSessionsRequest {})
            .await
            .map_err(|status| DrainError::rpc("list_sessions", status))?;
        resp.into_inner()
            .sessions
            .into_iter()
            .filter_map(|item| item.session)
            .map(|s| {
                Ok(UpgradeSession {
                    host_id: s.host_id.as_deref().map(parse_host_id).transpose()?,
                    id: s.id,
                    state: s.status,
                })
            })
            .collect()
    }

    /// Whether the session has a checkpoint it can resume from.
    pub async fn has_checkpoint(&self, id: &str) -> Result<bool, DrainError> {
        let resp = self
            .sessions
            .clone()
            .list_checkpoints(app::ListCheckpointsRequest {
                session_id: id.into(),
            })
            .await
            .map_err(|status| DrainError::rpc("list_checkpoints", status))?;
        Ok(resp.into_inner().checkpoints.iter().any(|c| c.recoverable))
    }

    pub async fn snapshot(&self, id: &str) -> Result<(), DrainError> {
        self.sessions
            .clone()
            .snapshot(app::SnapshotRequest {
                session_id: id.into(),
            })
            .await
            .map_err(|status| DrainError::rpc("snapshot", status))?;
        Ok(())
    }

    pub async fn resume(&self, id: &str) -> Result<(), DrainError> {
        self.sessions
            .clone()
            .resume(app::ResumeRequest {
                session_id: id.into(),
            })
            .await
            .map_err(|status| DrainError::rpc("resume", status))?;
        Ok(())
    }

    /// The eviction that does not park: the session ends `idle` behind a
    /// fresh checkpoint, its VM destroyed.
    pub async fn evict(&self, id: &str) -> Result<(), DrainError> {
        self.sessions
            .clone()
            .evict_local(app::EvictLocalRequest {
                session_id: id.into(),
            })
            .await
            .map_err(|status| DrainError::rpc("evict_local", status))?;
        Ok(())
    }
}
