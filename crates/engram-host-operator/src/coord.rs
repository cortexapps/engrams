//! ADR 0044 K3 / ADR 0051: app-gRPC client for the coordinator `FleetService`
//! the operator drives during a roll — cordon → drain → poll the drain gate
//! → uncordon — plus the scale-down wave's list/demand/delete.
//!
//! This was a REST (`reqwest`) client until the coordinator retired its
//! HTTP control plane (ADR 0051, gRPC-only). The REST routes the operator
//! called (`/api/v1/admin/hosts/:id/{cordon,uncordon,drain}`, `DELETE
//! /admin/hosts/:id`, `GET /hosts[/:id]`, `/admin/fleet/demand`) were
//! removed but the operator was never migrated, so every call 404'd and the
//! fleet silently stopped rolling. This client now talks `FleetService`.
//!
//! Auth: a service bearer token via `ENGRAM_COORDINATOR_TOKEN`, sent as
//! `Authorization: Bearer <token>` gRPC metadata (the same scheme the cli
//! uses). In a synthetic-admin dev coord (no accepted tokens) it's omitted.

use engram_core::HostId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use tonic::codegen::InterceptedService;
use tonic::transport::Channel;

use crate::error::OperatorError;
use crate::scaler::FleetDemand;

/// The `HostView` fields the drain/roll gates need (from
/// `FleetService.GetHost`).
#[derive(Debug)]
pub struct HostStatus {
    /// `"ready"` | `"draining"` | `"dead"` — the coordinator's view.
    #[allow(dead_code)]
    pub status: String,
    /// Live sandbox count from the host's latest heartbeat. The drain gate
    /// waits for this to reach 0.
    pub running_sandboxes: u32,
    /// ADR 0088: in-flight enable work bound to this host — enable_jobs
    /// live-materializing here, and non-terminal capture_jobs. Both roll
    /// paths wait on these reaching 0 before killing the host-agent pod
    /// (a materialize/capture is the host-agent process's OWN work; unlike
    /// session VMs it does not survive the pod swap).
    pub live_materializes: u32,
    pub live_capture_jobs: u32,
}

impl HostStatus {
    /// Any in-flight enable work a pod kill would destroy.
    pub fn has_enable_work(&self) -> bool {
        self.live_materializes > 0 || self.live_capture_jobs > 0
    }
}

/// One host's load + budget (from `FleetService.ListHosts`) — the fields the
/// scale-down wave planner's victim selection + 2D capacity guard read.
#[derive(Clone, Debug)]
pub struct HostLoad {
    pub id: HostId,
    pub cordoned: bool,
    pub running_sandboxes: u32,
    pub reserved_mib: u64,
    pub free_mib: u64,
    pub reserved_vcpus: u64,
    pub free_vcpus: u64,
}

/// `Authorization: Bearer <token>` gRPC metadata interceptor (mirrors the
/// cli's `BearerFn`). `None` omits the header — a dev coord with no accepted
/// tokens.
#[derive(Clone)]
struct BearerFn {
    token: Option<String>,
}

impl tonic::service::Interceptor for BearerFn {
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

type FleetClient = FleetServiceClient<InterceptedService<Channel, BearerFn>>;

/// Ensure the endpoint carries a scheme so `Channel::from_shared` parses it
/// — the CR's `coordinator_url` may be set scheme-less (mirrors the cli).
fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}

pub struct CoordClient {
    fleet: FleetClient,
}

impl CoordClient {
    /// `endpoint` is the coordinator's app-gRPC address (e.g.
    /// `http://engram-coordinator:50061`), from the CR's `coordinator_url`.
    /// Connects lazily (`connect_lazy`) so construction stays infallible +
    /// synchronous; a down coordinator surfaces as `Unavailable` on the
    /// first RPC (which the reconcile loop already requeues on).
    pub fn new(endpoint: String, token: Option<String>) -> Self {
        let endpoint = normalize_endpoint(&endpoint);
        let channel = Channel::from_shared(endpoint.clone())
            .unwrap_or_else(|e| panic!("invalid coordinator gRPC endpoint {endpoint:?}: {e}"))
            .connect_lazy();
        let fleet = FleetServiceClient::with_interceptor(channel, BearerFn { token });
        Self { fleet }
    }

    /// `FleetService.CordonHost` — stop the picker placing new sessions on
    /// the host.
    pub async fn cordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .cordon_host(app::CordonHostRequest {
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "cordon",
                status: Box::new(status),
            })?;
        Ok(())
    }

    /// ADR 0116 A-D2: `FleetService.BeginHostHandoff` — declare the
    /// planned roll so the coordinator's binding-lease deadline covers
    /// the whole pod replacement. UNIMPLEMENTED-tolerant: an older
    /// coordinator predates the RPC; the roll proceeds under its legacy
    /// cordon shield (warn, `Ok(false)`). Any other failure is an error
    /// the caller treats exactly like a cordon failure — a roll must
    /// never delete the pod without a durable deadline in PG once the
    /// coordinator supports one.
    pub async fn handoff(&self, host: HostId, ttl_secs: u64) -> Result<bool, OperatorError> {
        match self
            .fleet
            .clone()
            .begin_host_handoff(app::BeginHostHandoffRequest {
                host_id: host.to_string(),
                ttl_secs,
            })
            .await
        {
            Ok(resp) => Ok(resp.into_inner().accepted),
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                tracing::warn!(%host,
                    "coordinator predates BeginHostHandoff; rolling under the legacy cordon shield");
                Ok(false)
            }
            Err(status) => Err(OperatorError::Rpc {
                op: "handoff",
                status: Box::new(status),
            }),
        }
    }

    /// `FleetService.UncordonHost`.
    pub async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .uncordon_host(app::UncordonHostRequest {
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "uncordon",
                status: Box::new(status),
            })?;
        Ok(())
    }

    /// `FleetService.AdminDrainHost` — the cordon+evacuate admin drain (the
    /// old `POST /admin/hosts/:id/drain`), NOT the soft member-facing
    /// `DrainHost` status flip. Returns the evacuating-session set; the wave
    /// observes actual progress via [`Self::host_status`], so we ignore it.
    pub async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .admin_drain_host(app::AdminDrainHostRequest {
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "drain",
                status: Box::new(status),
            })?;
        Ok(())
    }

    /// `FleetService.DeleteHost` (the old `DELETE /admin/hosts/:id`, ADR
    /// 0048) — deregister a drained host. The coordinator returns
    /// `FAILED_PRECONDITION` if any session is still bound, so the wave only
    /// calls this after the drain gate reports 0 sandboxes.
    pub async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .delete_host(app::DeleteHostRequest {
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "delete_host",
                status: Box::new(status),
            })?;
        Ok(())
    }

    /// `FleetService.ListHosts` — every host's load + budget, for the
    /// scale-down wave planner's victim selection + 2D capacity guard.
    pub async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
        let resp = self
            .fleet
            .clone()
            .list_hosts(app::ListHostsRequest {})
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "list_hosts",
                status: Box::new(status),
            })?;
        resp.into_inner()
            .hosts
            .into_iter()
            .map(host_view_to_load)
            .collect()
    }

    /// `FleetService.GetHost` — the drain gate. `Ok(None)` means the host is
    /// no longer registered (already gone — nothing left to drain), faithful
    /// to the old `GET /hosts/:id` 404 → `None`.
    pub async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        match self
            .fleet
            .clone()
            .get_host(app::GetHostRequest {
                host_id: host.to_string(),
            })
            .await
        {
            Ok(resp) => Ok(resp.into_inner().host.map(|v| HostStatus {
                status: v.status,
                running_sandboxes: v.running_sandboxes,
                live_materializes: v.live_materializes,
                live_capture_jobs: v.live_capture_jobs,
            })),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(OperatorError::Rpc {
                op: "host_status",
                status: Box::new(status),
            }),
        }
    }

    /// `FleetService.GetFleetDemand` — the K4 autoscaler's input.
    pub async fn fleet_demand(&self) -> Result<FleetDemand, OperatorError> {
        let resp = self
            .fleet
            .clone()
            .get_fleet_demand(app::GetFleetDemandRequest {})
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "fleet_demand",
                status: Box::new(status),
            })?
            .into_inner();
        Ok(FleetDemand {
            schedulable_hosts: resp.schedulable_hosts,
            free_mib: resp.free_mib,
            total_mib: resp.total_mib,
            free_vcpus: resp.free_vcpus,
            total_vcpus: resp.total_vcpus,
            queued_sessions: resp.queued_sessions,
            queued_mib: resp.queued_mib,
            queued_vcpus: resp.queued_vcpus,
        })
    }
}

/// proto `HostView` → the operator's `HostLoad` (the wave-relevant subset).
fn host_view_to_load(v: app::HostView) -> Result<HostLoad, OperatorError> {
    let id: HostId = v.id.parse().map_err(|_| {
        OperatorError::Invalid(format!("coordinator returned malformed host id {:?}", v.id))
    })?;
    Ok(HostLoad {
        id,
        cordoned: v.cordoned,
        running_sandboxes: v.running_sandboxes,
        reserved_mib: v.reserved_mib,
        free_mib: v.free_mib,
        reserved_vcpus: v.reserved_vcpus,
        free_vcpus: v.free_vcpus,
    })
}
