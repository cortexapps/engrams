//! ADR 0044 K3 / ADR 0039: gRPC client for the coordinator's app FleetService,
//! which the operator drives during a roll/wave — cordon → drain → poll the
//! drain gate → uncordon — and for the autoscaler's demand signal.
//!
//! ADR 0039: the coordinator's HTTP admin surface is gone; the operator talks
//! to the app-gRPC `FleetService` (bearer-authed). `coordinator_url` must be the
//! app-gRPC endpoint (e.g. `http://coord:50061`), NOT the old `/api/v1` HTTP
//! port. Prod carries a service bearer via `ENGRAM_COORDINATOR_TOKEN`; a
//! synthetic-admin dev coord (no configured tokens) needs none.

use engram_core::HostId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use serde::Deserialize;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

use crate::error::OperatorError;

/// The host fields the drain gate needs (from `GetHost`). The gate waits for
/// `running_sandboxes` to reach 0.
#[derive(Debug)]
pub struct HostStatus {
    /// `"ready"` | `"draining"` | … — the coordinator's view of the host.
    #[allow(dead_code)]
    pub status: String,
    /// Live sandbox count from the host's latest heartbeat.
    pub running_sandboxes: u32,
}

/// One host's load + budget (from `ListHosts`), for the scale-down wave
/// planner's victim selection + 2D capacity guard.
#[derive(Clone, Debug, Deserialize)]
pub struct HostLoad {
    pub id: HostId,
    pub cordoned: bool,
    pub running_sandboxes: u32,
    pub reserved_mib: u64,
    pub free_mib: u64,
    pub reserved_vcpus: u64,
    pub free_vcpus: u64,
}

pub struct CoordClient {
    channel: Channel,
    token: Option<String>,
}

impl CoordClient {
    /// `endpoint` is the coordinator's app-gRPC base URI (e.g.
    /// `http://coord:50061`). Connection is lazy — established on the first RPC
    /// — so construction can't fail on a not-yet-ready coordinator.
    pub fn new(endpoint: String, token: Option<String>) -> Self {
        let channel = Channel::from_shared(endpoint.trim_end_matches('/').to_string())
            .expect("invalid coordinator gRPC endpoint")
            .connect_lazy();
        Self { channel, token }
    }

    fn client(&self) -> FleetServiceClient<Channel> {
        FleetServiceClient::new(self.channel.clone())
    }

    /// Wrap a request body with the bearer token in gRPC metadata (matches the
    /// coordinator's `grpc_app::auth::BearerAuth`, which reads `authorization:
    /// Bearer <token>`).
    fn req<T>(&self, body: T) -> Request<T> {
        let mut req = Request::new(body);
        if let Some(tok) = &self.token {
            let val: MetadataValue<_> = format!("Bearer {tok}")
                .parse()
                .expect("token bytes are valid ASCII");
            req.metadata_mut().insert("authorization", val);
        }
        req
    }

    /// Map a `tonic::Status` into the operator error (reusing the `Coord`
    /// variant — the gRPC code goes in `status`, the message in `body`).
    fn err(op: &'static str, s: tonic::Status) -> OperatorError {
        OperatorError::Coord {
            op,
            status: s.code() as u16,
            body: s.message().to_string(),
        }
    }

    /// `CordonHost` — stop the picker placing new sessions on the host.
    pub async fn cordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.client()
            .cordon_host(self.req(app::CordonHostRequest {
                host_id: host.to_string(),
            }))
            .await
            .map_err(|s| Self::err("cordon", s))?;
        Ok(())
    }

    /// `UncordonHost` — return the host to the picker's view.
    pub async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.client()
            .uncordon_host(self.req(app::UncordonHostRequest {
                host_id: host.to_string(),
            }))
            .await
            .map_err(|s| Self::err("uncordon", s))?;
        Ok(())
    }

    /// `AdminDrainHost` — cordon, then evacuate every active session
    /// (Evacuating = snapshot + warm-restore on a peer). The `evac_resumer`
    /// scanner completes each move; progress is observed via [`Self::host_status`].
    ///
    /// The node-removal drain path (ADR 0045 Phase E / ADR 0048 scale-down).
    /// Image rolls reattach and never drain (`reconcile::roll_node`); the
    /// caller is the wave executor (`autoscale`).
    pub async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
        self.client()
            .admin_drain_host(self.req(app::AdminDrainHostRequest {
                host_id: host.to_string(),
            }))
            .await
            .map_err(|s| Self::err("drain", s))?;
        Ok(())
    }

    /// `DeleteHost` (ADR 0048) — deregister a drained host immediately, so its
    /// row doesn't linger to the dead-host TTL. The coordinator returns
    /// `FAILED_PRECONDITION` if any session is still bound, so the wave only
    /// calls this after the drain gate reports 0 sandboxes.
    pub async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
        self.client()
            .delete_host(self.req(app::DeleteHostRequest {
                host_id: host.to_string(),
            }))
            .await
            .map_err(|s| Self::err("delete_host", s))?;
        Ok(())
    }

    /// `ListHosts` (ADR 0048) — every host's load + budget, for the scale-down
    /// wave planner's victim selection + 2D capacity guard.
    pub async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
        let resp = self
            .client()
            .list_hosts(self.req(app::ListHostsRequest {}))
            .await
            .map_err(|s| Self::err("list_hosts", s))?
            .into_inner();
        resp.hosts
            .into_iter()
            .map(|h| {
                Ok(HostLoad {
                    id: h.id.parse().map_err(|_| OperatorError::Invalid(format!(
                        "list_hosts: malformed host id {:?}",
                        h.id
                    )))?,
                    cordoned: h.cordoned,
                    running_sandboxes: h.running_sandboxes,
                    reserved_mib: h.reserved_mib,
                    free_mib: h.free_mib,
                    reserved_vcpus: h.reserved_vcpus,
                    free_vcpus: h.free_vcpus,
                })
            })
            .collect()
    }

    /// `GetHost` — the drain gate. `Ok(None)` means the host is no longer
    /// registered (already gone — nothing left to drain).
    pub async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        let resp = match self
            .client()
            .get_host(self.req(app::GetHostRequest {
                host_id: host.to_string(),
            }))
            .await
        {
            Ok(r) => r.into_inner(),
            Err(s) if s.code() == Code::NotFound => return Ok(None),
            Err(s) => return Err(Self::err("host_status", s)),
        };
        let host = resp
            .host
            .ok_or_else(|| OperatorError::Invalid("get_host: empty host in response".into()))?;
        Ok(Some(HostStatus {
            status: host.status,
            running_sandboxes: host.running_sandboxes,
        }))
    }

    /// `FleetDemand` — the K4 autoscaler's input (schedulable hosts + free/total
    /// guest RAM/vCPU reservation + the queue's aggregate demand).
    pub async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
        let d = self
            .client()
            .fleet_demand(self.req(app::FleetDemandRequest {}))
            .await
            .map_err(|s| Self::err("fleet_demand", s))?
            .into_inner();
        Ok(crate::scaler::FleetDemand {
            schedulable_hosts: d.schedulable_hosts,
            free_mib: d.free_mib,
            total_mib: d.total_mib,
            free_vcpus: d.free_vcpus,
            total_vcpus: d.total_vcpus,
            queued_sessions: d.queued_sessions,
            queued_mib: d.queued_mib,
            queued_vcpus: d.queued_vcpus,
        })
    }
}
