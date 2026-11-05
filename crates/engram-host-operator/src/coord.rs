//! The coordinator `FleetService` calls the operator makes for image rolls,
//! scaling, and durable host retirement (ADR 0044 K3, ADR 0123 E).
//!
//! Auth: `ENGRAM_COORDINATOR_TOKEN` as `Authorization: Bearer <token>`
//! metadata; a coordinator with no accepted tokens gets no header.

use std::time::Duration;

use engram_core::HostId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_upgrade_drain::Bearer;
use tonic::codegen::InterceptedService;
use tonic::transport::Channel;

use crate::autoscale::Retirement;
use crate::error::OperatorError;
use crate::scaler::FleetDemand;

pub const CORDON_OWNER: &str = "operator";

/// The `HostView` fields the roll gate needs (from
/// `FleetService.GetHost`).
#[derive(Debug)]
pub struct HostStatus {
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

type FleetClient = FleetServiceClient<InterceptedService<Channel, Bearer>>;

pub struct CoordClient {
    fleet: FleetClient,
}

impl CoordClient {
    /// `endpoint` is the coordinator's app-gRPC address (e.g.
    /// `http://engram-coordinator:50061`), from the CR's `coordinator_url`.
    /// It connects lazily; a down coordinator surfaces as `Unavailable` on
    /// the first RPC, which the reconcile loop requeues.
    pub fn new(endpoint: String, token: Option<String>) -> Self {
        let channel = engram_upgrade_drain::channel(&endpoint, Duration::from_secs(10));
        Self {
            fleet: FleetServiceClient::with_interceptor(channel, Bearer::new(token)),
        }
    }

    /// `FleetService.CordonHost` — stop the picker placing new sessions on
    /// the host.
    pub async fn cordon(&self, host: HostId, owner: &str) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .cordon_host(app::CordonHostRequest {
                owner: owner.into(),
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
    /// planned roll so the coordinator's binding-lease deadline covers the
    /// whole pod replacement. The lease is the only shield for a roll, so
    /// the caller treats any failure like a cordon failure and never deletes
    /// the pod without a durable deadline in PG.
    pub async fn handoff(&self, host: HostId, ttl_secs: u64) -> Result<bool, OperatorError> {
        self.fleet
            .clone()
            .begin_host_handoff(app::BeginHostHandoffRequest {
                host_id: host.to_string(),
                ttl_secs,
            })
            .await
            .map(|resp| resp.into_inner().accepted)
            .map_err(|status| OperatorError::Rpc {
                op: "handoff",
                status: Box::new(status),
            })
    }

    /// `FleetService.UncordonHost`.
    pub async fn uncordon(&self, host: HostId, owner: &str) -> Result<(), OperatorError> {
        self.fleet
            .clone()
            .uncordon_host(app::UncordonHostRequest {
                owner: owner.into(),
                host_id: host.to_string(),
            })
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "uncordon",
                status: Box::new(status),
            })?;
        Ok(())
    }

    /// Request or observe the coordinator's durable retirement grant.
    pub async fn retire_host(
        &self,
        host: HostId,
        owner: &str,
        reason: &str,
    ) -> Result<Retirement, OperatorError> {
        match self
            .fleet
            .clone()
            .retire_host(app::RetireHostRequest {
                host_id: host.to_string(),
                owner: owner.into(),
                reason: reason.into(),
            })
            .await
        {
            Ok(response) => Ok(retirement_from_response(response.into_inner())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(Retirement::NoRow),
            Err(status) if status.code() == tonic::Code::FailedPrecondition => {
                Ok(Retirement::Foreign)
            }
            Err(status) => Err(OperatorError::Rpc {
                op: "retire_host",
                status: Box::new(status),
            }),
        }
    }

    /// Delete the coordinator row after granted cloud removal.
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
        self.host_views()
            .await?
            .into_iter()
            .map(host_view_to_load)
            .collect()
    }

    async fn host_views(&self) -> Result<Vec<app::HostView>, OperatorError> {
        let resp = self
            .fleet
            .clone()
            .list_hosts(app::ListHostsRequest {})
            .await
            .map_err(|status| OperatorError::Rpc {
                op: "list_hosts",
                status: Box::new(status),
            })?;
        Ok(resp.into_inner().hosts)
    }

    /// Read the host for the image roll enable-work gate.
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
    Ok(HostLoad {
        id: engram_upgrade_drain::parse_host_id(&v.id)?,
        cordoned: v.cordoned,
        running_sandboxes: v.running_sandboxes,
        reserved_mib: v.reserved_mib,
        free_mib: v.free_mib,
        reserved_vcpus: v.reserved_vcpus,
        free_vcpus: v.free_vcpus,
    })
}

fn retirement_from_response(response: app::RetireHostResponse) -> Retirement {
    let retirement = response.retirement.unwrap_or_default();
    if !retirement.retired_at.is_empty() {
        Retirement::Granted
    } else {
        Retirement::Pending(retirement.blockers.into_iter().map(|b| b.kind).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_response_requires_nonempty_retired_at() {
        assert_eq!(
            retirement_from_response(app::RetireHostResponse::default()),
            Retirement::Pending(vec![])
        );
        let mut response = app::RetireHostResponse {
            retirement: Some(app::HostRetirement {
                requested_at: "2026-10-05T00:00:00Z".into(),
                blockers: vec![app::RetirementBlocker {
                    kind: "bound_sessions".into(),
                    count: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            retirement_from_response(response.clone()),
            Retirement::Pending(vec!["bound_sessions".into()])
        );
        response.retirement.as_mut().unwrap().retired_at = "2026-10-05T00:01:00Z".into();
        assert_eq!(retirement_from_response(response), Retirement::Granted);
    }
}
