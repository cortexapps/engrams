//! ADR 0123 E: durable host retirement before cloud removal.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use engram_core::traits::cloud::NodePoolScaler;
use engram_core::HostId;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;

use crate::coord::{CoordClient, HostLoad, HostStatus, CORDON_OWNER};
use crate::crd::HostFleetSpec;
use crate::error::OperatorError;
use crate::reconcile::PodInfo;
use crate::reconcile::ROLL_STUCK_ANNOTATION;
use crate::scaler::{desired_hosts, AutoscalePolicy};
use crate::wave::{plan_wave, WaveHost, WavePolicy};

// Upgrade only with no old scale-down wave in flight. Do not read the old key.
pub const VICTIM_ANNOTATION: &str = "fleet.engram.io/victim";
/// Keeps the Node object, and the victim record on it, until the
/// coordinator row is deleted: the removal journal must outlive the cloud
/// instance.
pub const VICTIM_FINALIZER: &str = "fleet.engram.io/victim";

/// Unknown additive fields are tolerated so a rollback to an older
/// operator still recovers the record; unknown phases and kinds are not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VictimRecord {
    pub fleet: String,
    pub kind: VictimKind,
    pub phase: VictimPhase,
    #[serde(with = "deadline")]
    pub deadline: Option<SystemTime>,
    /// The Node was already unschedulable when the victim was marked, so
    /// the cordon is not ours to release.
    #[serde(default)]
    pub prior_unschedulable: bool,
}

mod deadline {
    use super::*;
    use k8s_openapi::jiff::Timestamp;
    pub fn serialize<S: serde::Serializer>(
        value: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .map(Timestamp::try_from)
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .map(|t| t.to_string())
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|s| {
                s.parse::<Timestamp>()
                    .map(SystemTime::from)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VictimKind {
    Shed,
    Repair,
}
impl VictimKind {
    fn label(self) -> &'static str {
        match self {
            Self::Shed => "shed",
            Self::Repair => "repair",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VictimPhase {
    Retiring,
    Removing,
}
#[derive(Clone, Debug)]
pub struct Victim {
    pub node: String,
    pub record: VictimRecord,
    /// The Node object carries a deletion timestamp: the cloud instance is
    /// gone and only our finalizer keeps the record alive.
    pub deleting: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retirement {
    Pending(Vec<String>),
    Granted,
    NoRow,
    /// The coordinator refused the request: another owner holds the
    /// cordon, or the host is dead. The annotation is ours, the cordon is not.
    Foreign,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    NotInPlan,
    Deadline,
    Pressure,
    NoRow,
    Foreign,
}
impl ReleaseReason {
    fn label(self) -> &'static str {
        match self {
            Self::NotInPlan => "not_in_plan",
            Self::Deadline => "deadline",
            Self::Pressure => "pressure",
            Self::NoRow => "no_row",
            Self::Foreign => "foreign_cordon",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    CallRetire,
    Remove,
    Release(ReleaseReason),
    Hold,
}
/// A victim that is not yet removing always asks the coordinator first:
/// a grant that landed before a failed annotation write must still be
/// observed, never released because the retired host left the plan.
pub fn plan_victim(v: &VictimRecord) -> Next {
    match v.phase {
        VictimPhase::Removing => Next::Remove,
        VictimPhase::Retiring => Next::CallRetire,
    }
}
pub fn after_retire(
    v: &VictimRecord,
    r: Retirement,
    in_plan: bool,
    pressure_now: bool,
    now: SystemTime,
) -> Next {
    let shed = v.kind == VictimKind::Shed;
    match r {
        Retirement::Granted => Next::Remove,
        Retirement::NoRow if shed => Next::Release(ReleaseReason::NoRow),
        // Another owner holds the cordon: the annotation is ours, the
        // cordon is not. A repair victim keeps waiting for its host.
        Retirement::Foreign if shed => Next::Release(ReleaseReason::Foreign),
        Retirement::Pending(_) if shed && !in_plan => Next::Release(ReleaseReason::NotInPlan),
        Retirement::Pending(_) if shed && pressure_now => Next::Release(ReleaseReason::Pressure),
        Retirement::Pending(_) if shed && v.deadline.is_some_and(|d| now >= d) => {
            Next::Release(ReleaseReason::Deadline)
        }
        _ => Next::Hold,
    }
}

/// Shared durable identity for roll recovery and victim records.
pub fn fleet_key(spec: &HostFleetSpec) -> &str {
    spec.autoscaling
        .as_ref()
        .map(|a| a.node_pool.as_str())
        .unwrap_or(&spec.daemon_set.name)
}

/// Coordinator admin calls the wave drives. A trait so the executor runs
/// against a recording mock in tests (the live impl is [`CoordClient`]).
#[async_trait]
pub trait CoordApi: Send + Sync {
    async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError>;
    async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError>;
    async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError>;
    async fn uncordon(&self, host: HostId, owner: &str) -> Result<(), OperatorError>;
    async fn retire_host(
        &self,
        host: HostId,
        owner: &str,
        reason: &str,
    ) -> Result<Retirement, OperatorError>;
    async fn delete_host(&self, host: HostId) -> Result<(), OperatorError>;
}

#[async_trait]
impl CoordApi for CoordClient {
    async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
        CoordClient::fleet_demand(self).await
    }
    async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
        CoordClient::list_hosts(self).await
    }
    async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        CoordClient::host_status(self, host).await
    }
    async fn uncordon(&self, host: HostId, owner: &str) -> Result<(), OperatorError> {
        CoordClient::uncordon(self, host, owner).await
    }
    async fn retire_host(
        &self,
        host: HostId,
        owner: &str,
        reason: &str,
    ) -> Result<Retirement, OperatorError> {
        CoordClient::retire_host(self, host, owner, reason).await
    }
    async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
        CoordClient::delete_host(self, host).await
    }
}

/// K8s node operations the wave needs: cordon (unschedulable) + the victim
/// annotation. A trait for the same test-seam reason as [`CoordApi`].
#[async_trait]
pub trait NodeOps: Send + Sync {
    /// Empty fleet selects all records in this DaemonSet's node snapshot after autoscaling is removed.
    async fn victims(&self, fleet: &str) -> Result<Vec<Victim>, OperatorError>;
    /// One patch: the record, `spec.unschedulable = true`, and the victim
    /// finalizer that keeps the Node object (the removal journal) until
    /// `clear_victim`.
    async fn mark_victim(
        &self,
        node: &str,
        record: &VictimRecord,
    ) -> Result<VictimRecord, OperatorError>;
    /// Remove the record and the finalizer; `uncordon` also clears
    /// `spec.unschedulable` (false when the cordon was not ours).
    async fn clear_victim(&self, node: &str, uncordon: bool) -> Result<(), OperatorError>;

    /// The K8s Node object's `creationTimestamp`, for the node-ready
    /// bring-up histogram. Defaulted to `None` so the recording test
    /// mocks don't have to model it — a missing Node just skips the
    /// sample.
    async fn node_created_at(
        &self,
        _node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(None)
    }

    /// The Node's first current Ready transition. A missing condition skips
    /// only the phases that depend on it.
    async fn node_ready_at(
        &self,
        _node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(None)
    }
}

/// Live K8s implementation of [`NodeOps`].
pub struct K8sNodeOps<'a> {
    pub client: Client,
    /// One label-scoped, cacheable snapshot shared by all node reads in this
    /// reconcile. Writes still go directly to the API server.
    pub managed_nodes: &'a [Node],
}

#[async_trait]
impl NodeOps for K8sNodeOps<'_> {
    async fn victims(&self, fleet: &str) -> Result<Vec<Victim>, OperatorError> {
        use kube::ResourceExt;
        let mut victims = Vec::new();
        for node in self.managed_nodes {
            let Some(raw) = node.annotations().get(VICTIM_ANNOTATION) else {
                continue;
            };
            match serde_json::from_str::<VictimRecord>(raw) {
                Ok(record) if fleet.is_empty() || record.fleet == fleet => {
                    victims.push(Victim {
                        node: node.name_any(),
                        record,
                        deleting: node.metadata.deletion_timestamp.is_some(),
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(node = %node.name_any(), %error,
                        "invalid victim annotation; leaving node unchanged");
                    ::metrics::counter!(crate::metrics::AUTOSCALE_INVALID_VICTIMS_TOTAL)
                        .increment(1);
                }
            }
        }
        Ok(victims)
    }

    async fn mark_victim(
        &self,
        node: &str,
        record: &VictimRecord,
    ) -> Result<VictimRecord, OperatorError> {
        let snapshot = self
            .managed_nodes
            .iter()
            .find(|n| n.metadata.name.as_deref() == Some(node));
        let mut record = record.clone();
        // Never overwrite an invalid or foreign record from the snapshot.
        // A re-mark keeps the prior-cordon fact the first mark observed.
        match snapshot
            .and_then(|n| n.metadata.annotations.as_ref())
            .and_then(|a| a.get(VICTIM_ANNOTATION))
        {
            Some(raw) => {
                let old: VictimRecord =
                    serde_json::from_str(raw).map_err(|e| OperatorError::Invalid(e.to_string()))?;
                if old.fleet != record.fleet {
                    return Err(OperatorError::Invalid("foreign victim record".into()));
                }
                record.prior_unschedulable = old.prior_unschedulable;
            }
            None => {
                record.prior_unschedulable = snapshot
                    .and_then(|n| n.spec.as_ref())
                    .and_then(|s| s.unschedulable)
                    .unwrap_or(false);
            }
        }
        let finalizers = snapshot
            .and_then(|n| n.metadata.finalizers.clone())
            .unwrap_or_default();
        let nodes: Api<Node> = Api::all(self.client.clone());
        nodes
            .patch(
                node,
                &PatchParams::default(),
                &Patch::Merge(mark_victim_patch(&record, &finalizers)?),
            )
            .await?;
        Ok(record)
    }
    async fn clear_victim(&self, node: &str, uncordon: bool) -> Result<(), OperatorError> {
        let finalizers: Vec<String> = self
            .managed_nodes
            .iter()
            .find(|n| n.metadata.name.as_deref() == Some(node))
            .and_then(|n| n.metadata.finalizers.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f != VICTIM_FINALIZER)
            .collect();
        let mut patch = serde_json::json!({
            "metadata": { "annotations": { VICTIM_ANNOTATION: null }, "finalizers": finalizers },
        });
        if uncordon {
            patch["spec"] = serde_json::json!({ "unschedulable": false });
        }
        let nodes: Api<Node> = Api::all(self.client.clone());
        match nodes
            .patch(node, &PatchParams::default(), &Patch::Merge(patch))
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn node_created_at(
        &self,
        node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(self
            .managed_nodes
            .iter()
            .find(|n| n.metadata.name.as_deref() == Some(node))
            .and_then(|n| n.metadata.creation_timestamp.clone())
            .map(|t| t.0.into()))
    }

    async fn node_ready_at(
        &self,
        node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(self
            .managed_nodes
            .iter()
            .find(|n| n.metadata.name.as_deref() == Some(node))
            .and_then(|n| n.status.as_ref())
            .and_then(|s| s.conditions.as_ref())
            .and_then(|conditions| {
                conditions
                    .iter()
                    .find(|c| c.type_ == "Ready" && c.status == "True")
            })
            .and_then(|c| c.last_transition_time.clone())
            .map(|t| t.0.into()))
    }
}

/// The victim record, the cordon, and the finalizer that keeps the Node
/// object (this record's home) until `clear_victim`, in one patch.
fn mark_victim_patch(
    record: &VictimRecord,
    finalizers: &[String],
) -> Result<serde_json::Value, OperatorError> {
    let value = serde_json::to_string(record).map_err(|e| OperatorError::Invalid(e.to_string()))?;
    let mut finalizers = finalizers.to_vec();
    if !finalizers.iter().any(|f| f == VICTIM_FINALIZER) {
        finalizers.push(VICTIM_FINALIZER.into());
    }
    Ok(serde_json::json!({
        "metadata": {
            "annotations": { VICTIM_ANNOTATION: value, ROLL_STUCK_ANNOTATION: null },
            "finalizers": finalizers,
        },
        "spec": { "unschedulable": true }
    }))
}

/// Cross-tick memory for the node-ready histogram: the set of host ids
/// already observed registered with the coordinator. `None` until the
/// first observation — the first tick seeds the set WITHOUT emitting, so
/// an operator restart never reports pre-existing hosts as fresh joins.
#[derive(Default)]
pub struct NodeReadyTracker(std::sync::Mutex<NodeReadyState>);

#[derive(Default)]
struct NodeReadyState {
    registered: Option<std::collections::HashSet<HostId>>,
    pod_nodes: Option<std::collections::HashSet<String>>,
    pending_scale_requests: std::collections::VecDeque<std::time::SystemTime>,
    scale_request_by_node: HashMap<String, std::time::SystemTime>,
}

impl NodeReadyTracker {
    /// One observation of the registered-host set. Returns the hosts that
    /// are new since the previous observation — EMPTY on the very first
    /// call, which seeds the set instead (so an operator restart never
    /// reports pre-existing hosts as fresh joins). Pure state transition,
    /// split out from the async emission for direct unit testing.
    fn observe(&self, registered: std::collections::HashSet<HostId>) -> Vec<HostId> {
        let mut guard = self.0.lock().expect("node-ready tracker poisoned");
        match guard.registered.as_mut() {
            None => {
                guard.registered = Some(registered);
                Vec::new()
            }
            Some(seen) => {
                let fresh = registered.difference(seen).copied().collect();
                *seen = registered;
                fresh
            }
        }
    }

    fn observe_pods(&self, pods: &[PodInfo]) {
        let mut current: std::collections::HashSet<String> =
            pods.iter().map(|p| p.node.clone()).collect();
        let mut guard = self.0.lock().expect("node-ready tracker poisoned");
        let Some(previous) = guard.pod_nodes.as_ref() else {
            guard.pod_nodes = Some(current);
            return;
        };
        let mut fresh: Vec<String> = current.difference(previous).cloned().collect();
        fresh.sort();
        for node in fresh {
            if let Some(requested_at) = guard.pending_scale_requests.pop_front() {
                guard.scale_request_by_node.insert(node, requested_at);
            }
        }
        guard
            .scale_request_by_node
            .retain(|node, _| current.contains(node));
        guard.pod_nodes = Some(std::mem::take(&mut current));
    }

    fn record_grow(&self, physical: u32, target: u32, requested_at: std::time::SystemTime) {
        let mut guard = self.0.lock().expect("node-ready tracker poisoned");
        let planned = physical.saturating_add(guard.pending_scale_requests.len() as u32);
        for _ in planned..target {
            guard.pending_scale_requests.push_back(requested_at);
        }
    }

    fn take_scale_request(&self, node: &str) -> Option<std::time::SystemTime> {
        self.0
            .lock()
            .expect("node-ready tracker poisoned")
            .scale_request_by_node
            .remove(node)
    }
}

/// Minute-based scale-down hysteresis, independent of reconcile frequency.
///
/// Scale-up needs a short reconcile interval, but making the controller poll
/// faster must not also make scale-down more aggressive. The CRD keeps its
/// existing `scaleDownHysteresisTicks` surface; one tick is explicitly one
/// minute instead of one reconcile. The first observation counts as tick 1,
/// matching the old 60-second reconcile behavior.
#[derive(Default)]
pub struct ScaleDownHysteresis(std::sync::Mutex<ScaleDownHysteresisState>);

#[derive(Default)]
struct ScaleDownHysteresisState {
    ticks: u32,
    last_tick: Option<tokio::time::Instant>,
}

const SCALE_DOWN_HYSTERESIS_TICK: Duration = Duration::from_secs(60);

impl ScaleDownHysteresis {
    fn observe(&self, target: bool) -> u32 {
        self.observe_at(target, crate::time_source::metrics_now_tokio())
    }

    fn observe_at(&self, target: bool, now: tokio::time::Instant) -> u32 {
        let mut state = self.0.lock().expect("scale-down hysteresis poisoned");
        if !target {
            *state = ScaleDownHysteresisState::default();
            return 0;
        }

        match state.last_tick {
            None => {
                state.ticks = 1;
                state.last_tick = Some(now);
            }
            Some(last) => {
                if now.duration_since(last) >= SCALE_DOWN_HYSTERESIS_TICK {
                    state.ticks = state.ticks.saturating_add(1);
                    state.last_tick = Some(now);
                }
            }
        }
        state.ticks
    }
}

/// Emit `engram_node_ready_seconds` for every host newly visible in the
/// coordinator's list: K8s Node creation → registration, the whole
/// bring-up pipeline (instance create + kubelet join + assets staging +
/// register). Best-effort — a missing Node object skips the sample.
async fn track_node_ready(
    tracker: &NodeReadyTracker,
    nodes: &dyn NodeOps,
    pods: &[PodInfo],
    hosts: &[HostLoad],
) {
    tracker.observe_pods(pods);
    // The lock is confined to `observe` — never held across the async
    // Node lookups below.
    let fresh = tracker.observe(hosts.iter().map(|h| h.id).collect());
    for host in fresh {
        let Some(pod) = pods
            .iter()
            .find(|p| HostId::from_node_name(&p.node) == host)
        else {
            continue;
        };
        let registered_at = crate::time_source::wall_now();
        let scale_requested_at = tracker.take_scale_request(&pod.node);
        let created_at = match nodes.node_created_at(&pod.node).await {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(node = %pod.node, error = %e,
                    "node-created timestamp unavailable");
                None
            }
        };
        let ready_at = match nodes.node_ready_at(&pod.node).await {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(node = %pod.node, error = %e,
                    "node-ready timestamp unavailable");
                None
            }
        };
        let record = |phase: &'static str,
                      start: Option<std::time::SystemTime>,
                      end: Option<std::time::SystemTime>| {
            if let (Some(start), Some(end)) = (start, end) {
                if let Ok(elapsed) = end.duration_since(start) {
                    ::metrics::histogram!(crate::metrics::NODE_BOOTSTRAP_PHASE_SECONDS, "phase" => phase)
                        .record(elapsed.as_secs_f64());
                }
            }
        };
        record(
            "scale_request_to_node_created",
            scale_requested_at,
            created_at,
        );
        record("node_created_to_ready", created_at, ready_at);
        record(
            "ready_to_node_prep",
            ready_at,
            pod.startup.node_prep_started,
        );
        record(
            "node_prep",
            pod.startup.node_prep_started,
            pod.startup.node_prep_finished,
        );
        record(
            "node_prep_to_assets",
            pod.startup.node_prep_finished,
            pod.startup.assets_started,
        );
        record(
            "stage_node_assets",
            pod.startup.assets_started,
            pod.startup.assets_finished,
        );
        record(
            "assets_to_host_agent",
            pod.startup.assets_finished,
            pod.startup.host_agent_started,
        );
        record(
            "host_agent_to_registered",
            pod.startup.host_agent_started,
            Some(registered_at),
        );
        record(
            "scale_request_to_registered",
            scale_requested_at,
            Some(registered_at),
        );
        if let Some(created) = created_at {
            if let Ok(age) = registered_at.duration_since(created) {
                ::metrics::histogram!(crate::metrics::NODE_READY_SECONDS).record(age.as_secs_f64());
                tracing::info!(node = %pod.node, %host, age_secs = age.as_secs(),
                    "node bring-up complete: host registered");
            }
        }
    }
}

/// Result of one autoscale step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoscaleStatus {
    /// A scale-down wave is draining victims, or stuck-roll remediation
    /// mutated the fleet this tick. The reconcile loop must not start an
    /// image roll and should requeue soon.
    ///
    /// Deliberately NOT set for queue/scale-up pressure or for mere
    /// stuck-roll debt (issue #1012): a roll is a reattach pod swap that
    /// removes no capacity, one-roll-at-a-time is enforced by `plan_roll`'s
    /// not-Ready arm, and the floor gate protects capacity. A queue starved
    /// by wire skew is served BY the roll — blocking on it deadlocks the
    /// fleet (the queue waits for the roll, the roll waits for the queue).
    pub blocks_roll: bool,
}

/// K8s/roll state observed by one stateless autoscale reconcile.
pub struct FleetObservation<'a> {
    pub pods: &'a [PodInfo],
    pub roll_idle: bool,
    pub stuck_rolls: &'a [String],
}

/// Map each pod's node name → its coordinator [`HostLoad`] (joined by the
/// deterministic `HostId::from_node_name`), building the [`WaveHost`] snapshot
/// the planner consumes. Nodes with no matching coord host (just joined,
/// already gone) are skipped. `annotated` marks the in-flight victims `pinned`.
pub fn assemble_wave_hosts(
    pods: &[PodInfo],
    coord_hosts: &[HostLoad],
    annotated: &[String],
) -> Vec<WaveHost> {
    let by_id: HashMap<HostId, &HostLoad> = coord_hosts.iter().map(|h| (h.id, h)).collect();
    pods.iter()
        .filter(|p| !p.node.is_empty())
        .filter_map(|p| {
            let host = by_id.get(&HostId::from_node_name(&p.node))?;
            let pinned = annotated.iter().any(|n| n == &p.node);
            // A host cordoned by something OTHER than this wave (a manual
            // operator cordon, or an image roll) is off-limits: don't pick it
            // as a fresh victim, and don't count its capacity as a survivor's
            // (it isn't accepting placements). Our own in-flight victims are
            // cordoned + annotated → they pass through as `pinned`.
            if host.cordoned && !pinned {
                return None;
            }
            Some(WaveHost {
                node: p.node.clone(),
                running_sandboxes: host.running_sandboxes,
                reserved_mib: host.reserved_mib,
                reserved_vcpus: host.reserved_vcpus,
                free_mib: host.free_mib,
                free_vcpus: host.free_vcpus,
                pinned,
            })
        })
        .collect()
}

/// Release only our durable intent. A transient error preserves the record.
async fn release(
    coord: &dyn CoordApi,
    nodes: &dyn NodeOps,
    v: &Victim,
    reason: ReleaseReason,
) -> Result<(), OperatorError> {
    if reason != ReleaseReason::Foreign {
        match coord
            .uncordon(HostId::from_node_name(&v.node), CORDON_OWNER)
            .await
        {
            Ok(()) => {}
            Err(OperatorError::Rpc { status, .. })
                if matches!(
                    status.code(),
                    tonic::Code::NotFound | tonic::Code::FailedPrecondition
                ) => {}
            Err(e) => return Err(e),
        }
    }
    nodes
        .clear_victim(&v.node, !v.record.prior_unschedulable)
        .await?;
    ::metrics::counter!(crate::metrics::AUTOSCALE_VICTIMS_RELEASED_TOTAL, "reason" => reason.label()).increment(1);
    Ok(())
}

/// One bounded step per victim. The annotation is the restart cursor.
pub async fn step(
    spec: &HostFleetSpec,
    scaler: Option<&dyn NodePoolScaler>,
    scaledown_hysteresis: &ScaleDownHysteresis,
    node_ready: &NodeReadyTracker,
    coord: &dyn CoordApi,
    nodes: &dyn NodeOps,
    observation: FleetObservation<'_>,
) -> Result<AutoscaleStatus, OperatorError> {
    let demand = coord.fleet_demand().await?;
    let hosts = coord.list_hosts().await?;
    let key = fleet_key(spec);
    let mut victims = nodes
        .victims(if spec.autoscaling.is_some() { key } else { "" })
        .await?;
    let policy = spec.autoscaling.as_ref().map(|a| AutoscalePolicy {
        min_hosts: a.min_hosts,
        max_hosts: a.max_hosts,
        target_free_mib: a.target_free_mib,
        scale_down: a.scale_down,
    });
    let desired = policy
        .map(|p| desired_hosts(demand, p))
        .unwrap_or(demand.schedulable_hosts);
    let Some(scaler) = scaler else {
        if let Some(a) = &spec.autoscaling {
            let pinned: Vec<_> = victims.iter().map(|v| v.node.clone()).collect();
            let decision = plan_wave(
                &assemble_wave_hosts(observation.pods, &hosts, &pinned),
                desired,
                WavePolicy {
                    mode: a.scale_down,
                    max_shed_per_wave: a.max_shed_per_wave.max(1),
                    floor: a.min_hosts.max(spec.capacity_floor),
                    headroom_mib: a.target_free_mib,
                },
            );
            tracing::info!(desired, victims = ?decision.victims, note = %decision.note, "autoscale observe-only plan");
        }
        return Ok(AutoscaleStatus::default());
    };
    track_node_ready(node_ready, nodes, observation.pods, &hosts).await;
    let mut planned = Vec::new();
    let mut repaired = false;
    if let Some(a) = &spec.autoscaling {
        let physical = observation.pods.len() as u32;
        let unavailable = observation.stuck_rolls.len() as u32
            + victims
                .iter()
                .filter(|v| {
                    v.record.kind == VictimKind::Repair
                        && !observation.stuck_rolls.contains(&v.node)
                })
                .count() as u32;
        let grow_target = desired
            .saturating_add(unavailable)
            .min(a.max_hosts.max(a.min_hosts));
        for (kind, value) in [
            ("desired", desired),
            ("schedulable", demand.schedulable_hosts),
            ("physical", physical),
            ("grow_target", grow_target),
        ] {
            ::metrics::gauge!(crate::metrics::AUTOSCALE_HOSTS, "kind" => kind).set(value as f64);
        }
        if grow_target > physical && grow_target > scaler.current_target(&a.node_pool).await? {
            scaler.set_size(&a.node_pool, grow_target).await?;
            node_ready.record_grow(physical, grow_target, crate::time_source::wall_now());
            ::metrics::counter!(crate::metrics::AUTOSCALE_GROWS_TOTAL).increment(1);
        }
        let budget = a.max_shed_per_wave.max(1) as usize;
        if demand.queued_sessions == 0 && demand.schedulable_hosts >= desired {
            for node in observation.stuck_rolls {
                if victims.len() >= budget {
                    break;
                }
                if victims.iter().any(|v| &v.node == node) {
                    continue;
                }
                let record = VictimRecord {
                    fleet: key.into(),
                    kind: VictimKind::Repair,
                    phase: VictimPhase::Retiring,
                    deadline: None,
                    prior_unschedulable: false,
                };
                let record = match nodes.mark_victim(node, &record).await {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::warn!(%node, %error, "could not mark repair victim; retry next tick");
                        continue;
                    }
                };
                victims.push(Victim {
                    node: node.clone(),
                    record,
                    deleting: false,
                });
                repaired = true;
            }
        }
        let pinned: Vec<_> = victims
            .iter()
            .filter(|v| {
                v.record.kind == VictimKind::Shed && v.record.phase == VictimPhase::Retiring
            })
            .map(|v| v.node.clone())
            .collect();
        let reserved = victims
            .iter()
            .filter(|v| {
                v.record.kind == VictimKind::Repair || v.record.phase == VictimPhase::Removing
            })
            .count();
        let mut wave_hosts = assemble_wave_hosts(observation.pods, &hosts, &pinned);
        wave_hosts.retain(|h| {
            !victims.iter().any(|v| {
                v.node == h.node
                    && (v.record.kind == VictimKind::Repair
                        || v.record.phase == VictimPhase::Removing)
            })
        });
        if a.scale_down.enabled() {
            planned = plan_wave(
                &wave_hosts,
                desired,
                WavePolicy {
                    mode: a.scale_down,
                    max_shed_per_wave: budget.saturating_sub(reserved) as u32,
                    floor: a.min_hosts.max(spec.capacity_floor),
                    headroom_mib: a.target_free_mib,
                },
            )
            .victims;
        }
        let target = a.scale_down.enabled()
            && desired < demand.schedulable_hosts
            && demand.queued_sessions == 0
            && unavailable == 0;
        let ticks = scaledown_hysteresis.observe(target && victims.is_empty());
        let can_start =
            target && observation.roll_idle && ticks >= a.scale_down_hysteresis_ticks.max(1);
        planned.retain(|node| pinned.contains(node) || can_start);
        for node in &planned {
            if victims.iter().any(|v| &v.node == node) {
                continue;
            }
            let record = VictimRecord {
                fleet: key.into(),
                kind: VictimKind::Shed,
                phase: VictimPhase::Retiring,
                deadline: Some(
                    crate::time_source::wall_now()
                        + Duration::from_secs(spec.drain_timeout_seconds),
                ),
                prior_unschedulable: false,
            };
            let record = match nodes.mark_victim(node, &record).await {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(%node, %error, "could not mark shed victim; retry next tick");
                    continue;
                }
            };
            victims.push(Victim {
                node: node.clone(),
                record,
                deleting: false,
            });
        }
    }
    victims.sort_by_key(|v| v.record.kind != VictimKind::Repair);
    let mut remaining = Vec::new();
    for mut v in victims {
        let result: Result<bool, OperatorError> = async {
            let host = HostId::from_node_name(&v.node);
            let mut next = plan_victim(&v.record);
            if next == Next::CallRetire {
                let retirement = coord.retire_host(host, CORDON_OWNER, v.record.kind.label()).await?;
                let pressure = if let Retirement::Pending(blockers) = &retirement {
                    for reason in blockers {
                        ::metrics::counter!(crate::metrics::AUTOSCALE_RETIRE_BLOCKED_TOTAL, "reason" => reason.clone()).increment(1);
                    }
                    let fresh = coord.fleet_demand().await?;
                    fresh.queued_sessions > 0 || policy.is_some_and(|p| desired_hosts(fresh, p) > fresh.schedulable_hosts)
                } else { false };
                next = after_retire(&v.record, retirement, planned.contains(&v.node), pressure, crate::time_source::wall_now());
            }
            match next {
                Next::Remove => {
                    if v.record.phase != VictimPhase::Removing {
                        v.record.phase = VictimPhase::Removing;
                        v.record = nodes.mark_victim(&v.node, &v.record).await?;
                    }
                    // The cloud accepts a removal before the instance is gone.
                    // The retired row stays until the Node object is being
                    // deleted (the instance really went away), so a host-agent
                    // that restarts in between re-registers as still retired.
                    // The finalizer set by mark_victim keeps the Node object,
                    // and this record on it, until DeleteHost has succeeded.
                    if !v.deleting {
                        // The record retains the pool even if autoscaling was removed from the CR.
                        scaler.remove_node(&v.record.fleet, &v.node).await?;
                        return Ok(false);
                    }
                    coord.delete_host(host).await?;
                    nodes.clear_victim(&v.node, false).await?;
                    ::metrics::counter!(crate::metrics::AUTOSCALE_VICTIMS_REMOVED_TOTAL, "kind" => v.record.kind.label()).increment(1);
                    Ok(true)
                }
                Next::Release(reason) => { release(coord, nodes, &v, reason).await?; Ok(true) }
                Next::Hold => Ok(false),
                Next::CallRetire => unreachable!("retirement was evaluated"),
            }
        }.await;
        match result {
            Ok(true) => {}
            Ok(false) => remaining.push(v),
            Err(e) => {
                tracing::warn!(node = %v.node, error = %e, "victim step failed; retaining intent for retry");
                remaining.push(v);
            }
        }
    }
    for (phase, label) in [
        (VictimPhase::Retiring, "retiring"),
        (VictimPhase::Removing, "removing"),
    ] {
        ::metrics::gauge!(crate::metrics::AUTOSCALE_VICTIMS, "phase" => label)
            .set(remaining.iter().filter(|v| v.record.phase == phase).count() as f64);
    }
    // A stuck roll with autoscaling disabled has no repair path; it still
    // blocks further rolls so the fleet does not lose more capacity.
    let stuck_without_repair = spec.autoscaling.is_none() && !observation.stuck_rolls.is_empty();
    Ok(AutoscaleStatus {
        blocks_roll: repaired || !remaining.is_empty() || stuck_without_repair,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaler::FleetDemand;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    #[test]
    fn node_ready_tracker_seeds_silently_then_reports_fresh() {
        let t = NodeReadyTracker::default();
        let a = HostId::from_node_name("gke-engrams-kvm-a");
        let b = HostId::from_node_name("gke-engrams-kvm-b");
        // First observation seeds without reporting (operator restart must
        // not emit stale bring-up samples for pre-existing hosts).
        assert!(t.observe([a].into_iter().collect()).is_empty());
        // A host new since the seed is reported exactly once.
        assert_eq!(t.observe([a, b].into_iter().collect()), vec![b]);
        assert!(t.observe([a, b].into_iter().collect()).is_empty());
    }

    #[test]
    fn node_ready_tracker_reports_rejoin_after_departure() {
        let t = NodeReadyTracker::default();
        let a = HostId::from_node_name("gke-engrams-kvm-a");
        let b = HostId::from_node_name("gke-engrams-kvm-b");
        assert!(t.observe([a, b].into_iter().collect()).is_empty());
        // b departs (scale-down)…
        assert!(t.observe([a].into_iter().collect()).is_empty());
        // …and a same-named node rejoining is a fresh bring-up again.
        assert_eq!(t.observe([a, b].into_iter().collect()), vec![b]);
    }

    #[test]
    fn node_ready_tracker_pairs_each_grow_with_one_new_pod() {
        fn pod(node: &str) -> PodInfo {
            PodInfo {
                name: format!("pod-{node}"),
                node: node.into(),
                host_image: "host:new".into(),
                init_image: "assets:new".into(),
                ready: true,
                startup: crate::reconcile::PodStartupTimes::default(),
            }
        }

        let tracker = NodeReadyTracker::default();
        tracker.observe_pods(&[pod("a")]);
        let requested_at = std::time::UNIX_EPOCH + Duration::from_secs(100);
        tracker.record_grow(1, 2, requested_at);
        // The one-second reconcile can reassert the same target many times.
        // It must not create duplicate pending samples.
        tracker.record_grow(1, 2, requested_at + Duration::from_secs(1));
        tracker.observe_pods(&[pod("a"), pod("b")]);
        assert_eq!(tracker.take_scale_request("b"), Some(requested_at));
        assert_eq!(tracker.take_scale_request("b"), None);

        tracker.observe_pods(&[pod("a")]);
        let second = requested_at + Duration::from_secs(60);
        tracker.record_grow(1, 2, second);
        tracker.observe_pods(&[pod("a"), pod("c")]);
        assert_eq!(tracker.take_scale_request("c"), Some(second));
    }

    #[test]
    fn scale_down_hysteresis_uses_minute_ticks_not_reconcile_count() {
        let h = ScaleDownHysteresis::default();
        let start = crate::time_source::metrics_now_tokio();

        assert_eq!(h.observe_at(true, start), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(10)), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(59)), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(60)), 2);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(180)), 3);

        assert_eq!(h.observe_at(false, start + Duration::from_secs(181)), 0);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(182)), 1);
    }

    #[derive(Default)]
    struct Rec {
        log: Mutex<Vec<String>>,
        record_reads: bool,
        demand: Mutex<FleetDemand>,
        demands: Mutex<VecDeque<FleetDemand>>,
        retirements: Mutex<HashMap<HostId, VecDeque<Retirement>>>,
        victims: Mutex<Vec<Victim>>,
        hosts: Mutex<Vec<HostLoad>>,
        fail_remove: Mutex<bool>,
        fail_delete: Mutex<bool>,
        uncordon_result: Mutex<Option<tonic::Code>>,
        clear_victim_fails: Mutex<bool>,
        mark_victim_fails: Mutex<bool>,
        /// Nodes whose object carries a deletion timestamp.
        deleting: Mutex<std::collections::HashSet<String>>,
    }
    impl Rec {
        fn push(&self, s: impl Into<String>) {
            self.log.lock().unwrap().push(s.into());
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    fn rpc_error(op: &'static str, code: tonic::Code) -> OperatorError {
        OperatorError::Rpc {
            op,
            status: Box::new(tonic::Status::new(code, "injected failure")),
        }
    }
    #[async_trait]
    impl CoordApi for Arc<Rec> {
        async fn fleet_demand(&self) -> Result<FleetDemand, OperatorError> {
            if self.record_reads {
                self.push("fleet_demand");
            }
            Ok(self
                .demands
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(*self.demand.lock().unwrap()))
        }
        async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
            if self.record_reads {
                self.push("list_hosts");
            }
            Ok(self.hosts.lock().unwrap().clone())
        }
        async fn host_status(&self, _: HostId) -> Result<Option<HostStatus>, OperatorError> {
            panic!("retirement must not inspect heartbeat counts")
        }
        async fn retire_host(
            &self,
            host: HostId,
            owner: &str,
            reason: &str,
        ) -> Result<Retirement, OperatorError> {
            assert_eq!(owner, CORDON_OWNER);
            self.push(format!("retire_host {host} {reason}"));
            Ok(self
                .retirements
                .lock()
                .unwrap()
                .get_mut(&host)
                .and_then(|q| q.pop_front())
                .unwrap_or(Retirement::Pending(vec!["bound_sessions".into()])))
        }
        async fn uncordon(&self, host: HostId, owner: &str) -> Result<(), OperatorError> {
            assert_eq!(owner, CORDON_OWNER);
            self.push(format!("uncordon {host}"));
            match *self.uncordon_result.lock().unwrap() {
                None => Ok(()),
                Some(code) => Err(rpc_error("uncordon", code)),
            }
        }
        async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
            self.push(format!("delete_host {host}"));
            if *self.fail_delete.lock().unwrap() {
                Err(rpc_error("delete_host", tonic::Code::Unavailable))
            } else {
                Ok(())
            }
        }
    }
    #[async_trait]
    impl NodeOps for Arc<Rec> {
        async fn victims(&self, fleet: &str) -> Result<Vec<Victim>, OperatorError> {
            Ok(self
                .victims
                .lock()
                .unwrap()
                .iter()
                .filter(|v| fleet.is_empty() || v.record.fleet == fleet)
                .cloned()
                .map(|mut v| {
                    v.deleting = self.deleting.lock().unwrap().contains(&v.node);
                    v
                })
                .collect())
        }
        async fn mark_victim(
            &self,
            node: &str,
            record: &VictimRecord,
        ) -> Result<VictimRecord, OperatorError> {
            if *self.mark_victim_fails.lock().unwrap() {
                return Err(OperatorError::Invalid("patch failed".into()));
            }
            let patch = mark_victim_patch(record, &[])?;
            assert_eq!(patch["spec"]["unschedulable"], true);
            assert_eq!(patch["metadata"]["finalizers"][0], VICTIM_FINALIZER);
            assert!(patch["metadata"]["annotations"]
                .as_object()
                .unwrap()
                .contains_key(ROLL_STUCK_ANNOTATION));
            assert!(patch["metadata"]["annotations"][ROLL_STUCK_ANNOTATION].is_null());
            self.push(format!("mark_victim {node} {:?}", record.phase));
            let mut victims = self.victims.lock().unwrap();
            let prior = victims
                .iter()
                .find(|v| v.node == node)
                .map(|v| v.record.prior_unschedulable);
            victims.retain(|v| v.node != node);
            let mut record = record.clone();
            if let Some(prior) = prior {
                record.prior_unschedulable = prior;
            }
            victims.push(Victim {
                node: node.into(),
                record: record.clone(),
                deleting: self.deleting.lock().unwrap().contains(node),
            });
            Ok(record)
        }
        async fn clear_victim(&self, node: &str, uncordon: bool) -> Result<(), OperatorError> {
            self.push(format!("clear_victim {node}"));
            if !uncordon {
                self.push(format!("keep_cordon {node}"));
            }
            if *self.clear_victim_fails.lock().unwrap() {
                return Err(OperatorError::Invalid("clear failed".into()));
            }
            self.victims.lock().unwrap().retain(|v| v.node != node);
            Ok(())
        }
    }
    struct RecScaler {
        rec: Arc<Rec>,
        cloud_target: Mutex<u32>,
        target_reads: Mutex<Vec<String>>,
    }
    impl RecScaler {
        /// A scaler whose cloud already holds `cloud_target` nodes.
        fn new(rec: Arc<Rec>, cloud_target: u32) -> Self {
            Self {
                rec,
                cloud_target: Mutex::new(cloud_target),
                target_reads: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl NodePoolScaler for RecScaler {
        async fn current_target(&self, pool: &str) -> Result<u32, engram_core::BackendError> {
            self.target_reads.lock().unwrap().push(pool.into());
            Ok(*self.cloud_target.lock().unwrap())
        }

        async fn set_size(
            &self,
            _pool: &str,
            desired: u32,
        ) -> Result<(), engram_core::BackendError> {
            self.rec.push(format!("set_size {desired}"));
            Ok(())
        }
        async fn remove_node(
            &self,
            _pool: &str,
            node: &str,
        ) -> Result<(), engram_core::BackendError> {
            if *self.rec.fail_remove.lock().unwrap() {
                return Err(engram_core::BackendError::Protocol(format!(
                    "injected remove_node failure for {node}"
                )));
            }
            self.rec.push(format!("remove_node {node}"));
            Ok(())
        }
    }

    fn autoscale_spec() -> HostFleetSpec {
        HostFleetSpec {
            daemon_set: crate::crd::DaemonSetRef {
                namespace: "engrams-hosts".into(),
                name: "hf-host-agent".into(),
            },
            image: "host:new".into(),
            node_assets_image: "assets:new".into(),
            coordinator_url: "http://coord".into(),
            capacity_floor: 2,
            drain_timeout_seconds: 1,
            enable_work_timeout_seconds: 0,
            autoscaling: Some(crate::crd::AutoscalingSpec {
                node_pool: "kvm".into(),
                min_hosts: 2,
                max_hosts: 6,
                target_free_mib: 24_576,
                scale_down: crate::scaler::ScaleDownMode::Off,
                scale_down_hysteresis_ticks: 3,
                max_shed_per_wave: 1,
            }),
        }
    }

    fn ready_pod(node: &str) -> PodInfo {
        PodInfo {
            name: format!("pod-{node}"),
            node: node.into(),
            host_image: "host:new".into(),
            init_image: "assets:new".into(),
            ready: true,
            startup: crate::reconcile::PodStartupTimes::default(),
        }
    }

    async fn check_grow(cloud_target: u32, pod_count: usize) -> (Vec<String>, Vec<String>) {
        let rec = Arc::new(Rec::default());
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 3,
            total_mib: 48_000,
            free_mib: 0,
            ..Default::default()
        };
        let mut spec = autoscale_spec();
        spec.autoscaling.as_mut().unwrap().target_free_mib = 16_000;
        let scaler = RecScaler::new(rec.clone(), cloud_target);
        let pods: Vec<_> = (0..pod_count).map(|n| ready_pod(&n.to_string())).collect();
        step(
            &spec,
            Some(&scaler),
            &ScaleDownHysteresis::default(),
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: true,
                stuck_rolls: &[],
            },
        )
        .await
        .unwrap();
        let reads = scaler.target_reads.lock().unwrap().clone();
        (rec.log(), reads)
    }

    #[tokio::test]
    async fn grow_never_lowers_cloud_target() {
        let (log, reads) = check_grow(5, 3).await;
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(reads, ["kvm"]);
    }

    #[tokio::test]
    async fn grow_raises_when_cloud_target_is_below() {
        let (log, reads) = check_grow(3, 3).await;
        assert_eq!(log, ["set_size 4"]);
        assert_eq!(reads, ["kvm"]);
    }

    #[tokio::test]
    async fn grow_skips_cloud_read_when_pods_already_meet_target() {
        let (log, reads) = check_grow(3, 4).await;
        assert!(log.is_empty(), "{log:?}");
        assert!(reads.is_empty());
    }

    #[tokio::test]
    async fn observe_only_performs_no_mutations() {
        let rec = Arc::new(Rec {
            record_reads: true,
            ..Default::default()
        });
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 5,
            total_mib: 80_000,
            free_mib: 80_000,
            ..Default::default()
        };
        let mut spec = autoscale_spec();
        let policy = spec.autoscaling.as_mut().unwrap();
        policy.scale_down = crate::scaler::ScaleDownMode::Aggressive;
        policy.scale_down_hysteresis_ticks = 1;
        let pods: Vec<_> = (0..5).map(|n| ready_pod(&n.to_string())).collect();
        *rec.hosts.lock().unwrap() = pods
            .iter()
            .map(|pod| HostLoad {
                id: HostId::from_node_name(&pod.node),
                cordoned: false,
                running_sandboxes: 0,
                reserved_mib: 0,
                free_mib: 16_000,
                reserved_vcpus: 0,
                free_vcpus: 8,
            })
            .collect();
        let wave_hosts = assemble_wave_hosts(&pods, &rec.hosts.lock().unwrap(), &[]);
        assert!(
            !plan_wave(
                &wave_hosts,
                2,
                WavePolicy {
                    mode: crate::scaler::ScaleDownMode::Aggressive,
                    max_shed_per_wave: 1,
                    floor: 2,
                    headroom_mib: 24_576,
                }
            )
            .victims
            .is_empty(),
            "the surplus fleet must have a removable victim"
        );
        let status = step(
            &spec,
            None,
            &ScaleDownHysteresis::default(),
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: true,
                stuck_rolls: &[],
            },
        )
        .await
        .unwrap();
        assert!(!status.blocks_roll);
        assert_eq!(rec.log(), ["fleet_demand", "list_hosts"]);
    }

    #[tokio::test]
    async fn queued_demand_surges_past_a_roll_stuck_physical_node() {
        let rec = Arc::new(Rec::default());
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 2,
            free_mib: 16_713,
            total_mib: 131_072,
            free_vcpus: 80,
            total_vcpus: 80,
            queued_sessions: 1,
            queued_mib: 24_576,
            queued_vcpus: 8,
        };
        let scaler = RecScaler::new(rec.clone(), 3);
        let hysteresis = ScaleDownHysteresis::default();
        let pods = vec![ready_pod("a"), ready_pod("b"), ready_pod("stuck")];

        let status = step(
            &autoscale_spec(),
            Some(&scaler),
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: false,
                stuck_rolls: &["stuck".into()],
            },
        )
        .await
        .expect("autoscale step");

        assert!(
            !status.blocks_roll,
            "queue pressure / stuck debt must not block image rolls (issue #1012)"
        );
        let log = rec.log();
        assert!(
            log.iter().any(|line| line == "set_size 4"),
            "desired 3 schedulable + 1 stuck debt must surge to 4: {log:?}"
        );
        assert!(
            !log.iter().any(|line| line.starts_with("retire_host ")),
            "queued creates own replacement capacity before repair: {log:?}"
        );
    }

    /// Issue #1012 regression: after a coord-first wire bump every host is
    /// skew-excluded from placement, sessions queue, and the roll is the only
    /// cure for the skew. Queue pressure with nothing to grow must
    /// not block the roll, or the fleet deadlocks: the queue waits for the
    /// roll, the roll waits for the queue.
    #[tokio::test]
    async fn skew_starved_queue_does_not_block_the_roll() {
        let rec = Arc::new(Rec::default());
        // Coordinator view mid-deploy: both hosts wire-skewed, so zero
        // schedulable capacity and two queued creates that cannot place.
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 0,
            free_mib: 0,
            total_mib: 0,
            free_vcpus: 0,
            total_vcpus: 0,
            queued_sessions: 2,
            queued_mib: 49_152,
            queued_vcpus: 16,
        };
        let scaler = RecScaler::new(rec.clone(), 3);
        let hysteresis = ScaleDownHysteresis::default();
        // Both pods Ready but running stale images (they need the roll).
        let stale = |node: &str| PodInfo {
            host_image: "host:old".into(),
            init_image: "assets:old".into(),
            ..ready_pod(node)
        };
        let pods = vec![stale("a"), stale("b")];

        let status = step(
            &autoscale_spec(),
            Some(&scaler),
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: false,
                stuck_rolls: &[],
            },
        )
        .await
        .expect("autoscale step");

        assert!(
            !status.blocks_roll,
            "a skew-starved queue must be served BY the roll, never block it"
        );
    }

    #[test]
    fn assemble_joins_by_host_id_and_excludes_foreign_cordons() {
        fn pod(node: &str) -> PodInfo {
            PodInfo {
                name: format!("p-{node}"),
                node: node.into(),
                host_image: "i".into(),
                init_image: "n".into(),
                ready: true,
                startup: crate::reconcile::PodStartupTimes::default(),
            }
        }
        fn load(node: &str, cordoned: bool) -> HostLoad {
            HostLoad {
                id: HostId::from_node_name(node),
                cordoned,
                running_sandboxes: 1,
                reserved_mib: 1_000,
                free_mib: 1_000,
                reserved_vcpus: 1,
                free_vcpus: 1,
            }
        }
        let pods = vec![pod("normal"), pod("foreign-cordon"), pod("our-victim")];
        let hosts = vec![
            load("normal", false),
            load("foreign-cordon", true), // cordoned, NOT annotated → excluded
            load("our-victim", true),     // cordoned + annotated → pinned
        ];
        let annotated = vec!["our-victim".to_string()];
        let assembled = assemble_wave_hosts(&pods, &hosts, &annotated);
        let nodes: Vec<&str> = assembled.iter().map(|h| h.node.as_str()).collect();
        assert!(nodes.contains(&"normal"));
        assert!(nodes.contains(&"our-victim"));
        assert!(
            !nodes.contains(&"foreign-cordon"),
            "a host cordoned by something other than this wave is off-limits"
        );
        assert!(
            assembled
                .iter()
                .find(|h| h.node == "our-victim")
                .unwrap()
                .pinned
        );
        assert!(
            !assembled
                .iter()
                .find(|h| h.node == "normal")
                .unwrap()
                .pinned
        );
    }

    fn record(kind: VictimKind, phase: VictimPhase) -> VictimRecord {
        VictimRecord {
            fleet: "kvm".into(),
            kind,
            phase,
            deadline: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(500)),
            prior_unschedulable: false,
        }
    }
    fn retiring() -> VictimRecord {
        record(VictimKind::Shed, VictimPhase::Retiring)
    }
    #[test]
    fn plan_victim_removing_is_never_released() {
        for kind in [VictimKind::Shed, VictimKind::Repair] {
            assert_eq!(
                plan_victim(&record(kind, VictimPhase::Removing)),
                Next::Remove
            );
        }
    }
    /// A retiring victim always asks the coordinator first; the plan only
    /// decides what a Pending answer means.
    #[test]
    fn plan_victim_retiring_always_asks_the_coordinator() {
        for kind in [VictimKind::Shed, VictimKind::Repair] {
            assert_eq!(
                plan_victim(&record(kind, VictimPhase::Retiring)),
                Next::CallRetire
            );
        }
        assert_eq!(
            after_retire(
                &retiring(),
                Retirement::Pending(vec![]),
                false,
                false,
                SystemTime::UNIX_EPOCH
            ),
            Next::Release(ReleaseReason::NotInPlan)
        );
        assert_eq!(
            after_retire(
                &retiring(),
                Retirement::Granted,
                false,
                false,
                SystemTime::UNIX_EPOCH
            ),
            Next::Remove
        );
        assert_eq!(
            after_retire(
                &retiring(),
                Retirement::Foreign,
                true,
                false,
                SystemTime::UNIX_EPOCH
            ),
            Next::Release(ReleaseReason::Foreign)
        );
        assert_eq!(
            after_retire(
                &record(VictimKind::Repair, VictimPhase::Retiring),
                Retirement::Foreign,
                false,
                false,
                SystemTime::UNIX_EPOCH
            ),
            Next::Hold
        );
    }
    #[test]
    fn after_retire_only_granted_reaches_remove() {
        for kind in [VictimKind::Shed, VictimKind::Repair] {
            for pressure in [true, false] {
                let v = record(kind, VictimPhase::Retiring);
                assert_eq!(
                    after_retire(
                        &v,
                        Retirement::Granted,
                        true,
                        pressure,
                        SystemTime::UNIX_EPOCH
                    ),
                    Next::Remove
                );
                for result in [
                    Retirement::Pending(vec![]),
                    Retirement::Pending(vec!["bound_sessions".into()]),
                    Retirement::NoRow,
                ] {
                    assert_ne!(
                        after_retire(&v, result, true, pressure, SystemTime::UNIX_EPOCH),
                        Next::Remove
                    );
                }
            }
        }
    }
    #[test]
    fn after_retire_pending_under_pressure_releases_shed_not_repair() {
        assert_eq!(
            after_retire(
                &retiring(),
                Retirement::Pending(vec![]),
                true,
                true,
                SystemTime::UNIX_EPOCH
            ),
            Next::Release(ReleaseReason::Pressure)
        );
        assert_eq!(
            after_retire(
                &record(VictimKind::Repair, VictimPhase::Retiring),
                Retirement::Pending(vec![]),
                true,
                true,
                SystemTime::UNIX_EPOCH
            ),
            Next::Hold
        );
    }
    #[test]
    fn after_retire_shed_deadline_releases_pending() {
        let v = retiring();
        assert_eq!(
            after_retire(
                &v,
                Retirement::Pending(vec![]),
                true,
                false,
                v.deadline.unwrap()
            ),
            Next::Release(ReleaseReason::Deadline)
        );
        assert_eq!(
            after_retire(&v, Retirement::Granted, true, true, v.deadline.unwrap()),
            Next::Remove
        );
    }
    #[test]
    fn after_retire_repair_never_times_out() {
        let v = record(VictimKind::Repair, VictimPhase::Retiring);
        assert_eq!(
            after_retire(
                &v,
                Retirement::Pending(vec![]),
                false,
                false,
                v.deadline.unwrap() + Duration::from_secs(10)
            ),
            Next::Hold
        );
        assert_eq!(
            after_retire(&v, Retirement::NoRow, true, true, v.deadline.unwrap()),
            Next::Hold
        );
    }
    #[test]
    fn victim_record_round_trips_json_and_rejects_garbage() {
        for kind in [VictimKind::Shed, VictimKind::Repair] {
            for phase in [VictimPhase::Retiring, VictimPhase::Removing] {
                let mut v = record(kind, phase);
                for deadline in [v.deadline, None] {
                    v.deadline = deadline;
                    let json = serde_json::to_string(&v).unwrap();
                    assert_eq!(serde_json::from_str::<VictimRecord>(&json).unwrap(), v);
                    if deadline.is_some() {
                        assert!(json.contains("1970-01-01T00:08:20Z"));
                    }
                }
            }
        }
        for raw in [
            "kvm",
            "{}",
            r#"{"fleet":"kvm","kind":"shed","phase":"oops","deadline":null}"#,
            r#"{"fleet":"kvm","kind":"shed","phase":"retiring","deadline":"bad"}"#,
        ] {
            assert!(serde_json::from_str::<VictimRecord>(raw).is_err());
        }
    }
    #[test]
    fn fleet_key_is_shared_by_roll_stuck_and_victim_markers() {
        let mut spec = autoscale_spec();
        assert_eq!(fleet_key(&spec), "kvm");
        let v = VictimRecord {
            fleet: fleet_key(&spec).into(),
            ..retiring()
        };
        let patch = mark_victim_patch(&v, &[]).unwrap();
        let parsed: VictimRecord = serde_json::from_value(
            serde_json::from_str(
                patch["metadata"]["annotations"][VICTIM_ANNOTATION]
                    .as_str()
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.fleet, fleet_key(&spec));
        spec.autoscaling = None;
        assert_eq!(fleet_key(&spec), "hf-host-agent");
    }

    fn fixture() -> (Arc<Rec>, HostFleetSpec, Vec<PodInfo>) {
        let rec = Arc::new(Rec::default());
        let mut spec = autoscale_spec();
        let a = spec.autoscaling.as_mut().unwrap();
        a.scale_down = crate::scaler::ScaleDownMode::IdleOnly;
        a.scale_down_hysteresis_ticks = 1;
        spec.drain_timeout_seconds = 600;
        *rec.demand.lock().unwrap() = FleetDemand {
            schedulable_hosts: 3,
            total_mib: 180_000,
            free_mib: 180_000,
            ..Default::default()
        };
        let pods: Vec<_> = ["a", "b", "c"].into_iter().map(ready_pod).collect();
        *rec.hosts.lock().unwrap() = pods
            .iter()
            .map(|p| HostLoad {
                id: HostId::from_node_name(&p.node),
                cordoned: false,
                running_sandboxes: 0,
                reserved_mib: 0,
                free_mib: 60_000,
                reserved_vcpus: 0,
                free_vcpus: 30,
            })
            .collect();
        (rec, spec, pods)
    }
    fn seed(rec: &Rec, node: &str, kind: VictimKind, phase: VictimPhase) {
        let mut record = record(kind, phase);
        record.deadline = if kind == VictimKind::Repair {
            None
        } else {
            Some(crate::time_source::wall_now() + Duration::from_secs(600))
        };
        rec.victims.lock().unwrap().push(Victim {
            node: node.into(),
            record,
            deleting: false,
        });
    }
    fn deleting(rec: &Rec, node: &str) {
        rec.deleting.lock().unwrap().insert(node.into());
    }
    fn script(rec: &Rec, node: &str, results: Vec<Retirement>) {
        rec.retirements
            .lock()
            .unwrap()
            .insert(HostId::from_node_name(node), results.into());
    }
    async fn tick(
        rec: &Arc<Rec>,
        spec: &HostFleetSpec,
        pods: &[PodInfo],
        stuck: &[String],
    ) -> AutoscaleStatus {
        step(
            spec,
            Some(&RecScaler::new(rec.clone(), 3)),
            &ScaleDownHysteresis::default(),
            &NodeReadyTracker::default(),
            rec,
            rec,
            FleetObservation {
                pods,
                roll_idle: true,
                stuck_rolls: stuck,
            },
        )
        .await
        .unwrap()
    }
    fn count(rec: &Rec, prefix: &str) -> usize {
        rec.log().iter().filter(|l| l.starts_with(prefix)).count()
    }
    fn no_remove(rec: &Rec) {
        assert_eq!(count(rec, "remove_node"), 0);
        assert_eq!(count(rec, "delete_host"), 0);
    }

    #[tokio::test]
    async fn mark_victim_is_one_patch_before_retire_host() {
        let (rec, spec, pods) = fixture();
        tick(&rec, &spec, &pods, &[]).await;
        let log = rec.log();
        assert_eq!(log[0], "mark_victim a Retiring");
        assert!(log[1].starts_with("retire_host"));
        assert_eq!(count(&rec, "mark_victim"), 1);
        assert_eq!(count(&rec, "retire_host"), 1);
    }
    #[tokio::test]
    async fn retire_pending_holds_without_remove_or_delete() {
        for result in [
            Retirement::Pending(vec!["bound_sessions".into()]),
            Retirement::Pending(vec![]),
            Retirement::NoRow,
        ] {
            let (rec, spec, pods) = fixture();
            script(&rec, "a", vec![result.clone()]);
            let status = tick(&rec, &spec, &pods, &[]).await;
            assert_eq!(status.blocks_roll, result != Retirement::NoRow);
            no_remove(&rec);
        }
    }
    /// The grant orders the cloud removal; the coordinator row is deleted
    /// only once the Node object is being deleted (the instance is gone),
    /// so a host-agent that restarts in between stays retired.
    #[tokio::test]
    async fn granted_writes_removing_before_remove_node_then_deletes_once_the_node_goes() {
        let (rec, spec, pods) = fixture();
        script(&rec, "a", vec![Retirement::Granted]);
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        let log = rec.log();
        let pos = |p: &str| log.iter().position(|l| l.starts_with(p)).unwrap();
        assert!(pos("retire_host") < pos("mark_victim a Removing"));
        assert!(pos("mark_victim a Removing") < pos("remove_node"));
        assert_eq!(count(&rec, "delete_host"), 0);
        assert_eq!(
            rec.victims.lock().unwrap()[0].record.phase,
            VictimPhase::Removing
        );
        deleting(&rec, "a");
        assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "remove_node"), 1);
        assert_eq!(count(&rec, "delete_host"), 1);
        assert_eq!(count(&rec, "retire_host"), 1);
        assert!(rec.victims.lock().unwrap().is_empty());
        assert_eq!(count(&rec, "uncordon"), 0);
    }
    #[tokio::test]
    async fn delete_host_error_keeps_victim_in_flight() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Removing);
        deleting(&rec, "a");
        *rec.fail_delete.lock().unwrap() = true;
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(
            rec.victims.lock().unwrap()[0].record.phase,
            VictimPhase::Removing
        );
        assert_eq!(count(&rec, "clear_victim"), 0);
        *rec.fail_delete.lock().unwrap() = false;
        tick(&rec, &spec, &pods, &[]).await;
        assert_eq!(count(&rec, "retire_host"), 0);
        assert_eq!(
            count(&rec, "remove_node"),
            0,
            "a deleting Node is past removal"
        );
        assert_eq!(count(&rec, "delete_host"), 2);
        assert!(rec.victims.lock().unwrap().is_empty());
    }
    /// A grant whose `removing` patch never landed is still observed on the
    /// next tick: the retired host is absent from ListHosts and the plan,
    /// but the victim asks the coordinator before any release.
    #[tokio::test]
    async fn grant_observed_before_release_when_the_host_left_the_plan() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "zz", VictimKind::Shed, VictimPhase::Retiring);
        script(&rec, "zz", vec![Retirement::Granted]);
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "uncordon"), 0);
        assert_eq!(count(&rec, "remove_node zz"), 1);
        assert_eq!(
            rec.victims.lock().unwrap()[0].record.phase,
            VictimPhase::Removing
        );
    }
    /// A cordon another owner holds is left in place: only the annotation
    /// is ours. A repair victim keeps waiting.
    #[tokio::test]
    async fn foreign_cordon_releases_only_the_annotation() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        script(&rec, "a", vec![Retirement::Foreign]);
        assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        // The coordinator cordon is theirs; the Node cordon we set is ours.
        assert_eq!(count(&rec, "uncordon"), 0);
        assert_eq!(count(&rec, "clear_victim a"), 1);
        assert_eq!(count(&rec, "keep_cordon a"), 0);
        no_remove(&rec);
        let (rec, spec, pods) = fixture();
        seed(&rec, "c", VictimKind::Repair, VictimPhase::Retiring);
        script(&rec, "c", vec![Retirement::Foreign]);
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "clear_victim"), 0);
    }
    /// A Node that was unschedulable before it became a victim keeps that
    /// cordon when the victim is released.
    #[tokio::test]
    async fn prior_node_cordon_survives_release() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        rec.victims.lock().unwrap()[0].record.prior_unschedulable = true;
        rec.demand.lock().unwrap().queued_sessions = 1;
        assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "uncordon"), 1);
        assert_eq!(count(&rec, "keep_cordon a"), 1);
    }
    #[tokio::test]
    async fn demand_burst_while_pending_releases_without_remove() {
        let (rec, spec, pods) = fixture();
        let initial = *rec.demand.lock().unwrap();
        *rec.demands.lock().unwrap() = [
            initial,
            FleetDemand {
                queued_sessions: 1,
                ..initial
            },
        ]
        .into();
        assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "uncordon"), 1);
        no_remove(&rec);
        assert!(rec.demands.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn restart_mid_removing_resumes_remove_and_delete() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Removing);
        tick(&rec, &spec, &pods, &[]).await;
        assert_eq!(count(&rec, "retire_host"), 0);
        assert_eq!(count(&rec, "uncordon"), 0);
        assert_eq!(count(&rec, "mark_victim"), 0);
        assert_eq!(count(&rec, "remove_node"), 1);
        assert_eq!(count(&rec, "delete_host"), 0);
        deleting(&rec, "a");
        tick(&rec, &spec, &pods, &[]).await;
        assert_eq!(count(&rec, "remove_node"), 1);
        assert_eq!(count(&rec, "delete_host"), 1);
        assert!(rec.victims.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn restart_mid_retiring_reissues_retire_host() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        tick(&rec, &spec, &pods, &[]).await;
        tick(&rec, &spec, &pods, &[]).await;
        assert_eq!(count(&rec, "retire_host"), 2);
        assert_eq!(count(&rec, "mark_victim"), 0);
        no_remove(&rec);
    }
    #[tokio::test]
    async fn partial_release_retains_intent() {
        for fail_uncordon in [false, true] {
            let (rec, mut spec, pods) = fixture();
            spec.autoscaling = None;
            seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
            if fail_uncordon {
                *rec.uncordon_result.lock().unwrap() = Some(tonic::Code::Unavailable);
            } else {
                *rec.clear_victim_fails.lock().unwrap() = true;
            }
            assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
            assert_eq!(rec.victims.lock().unwrap().len(), 1);
            if fail_uncordon {
                assert_eq!(count(&rec, "clear_victim"), 0);
            }
            *rec.uncordon_result.lock().unwrap() = None;
            *rec.clear_victim_fails.lock().unwrap() = false;
            assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
            assert!(rec.victims.lock().unwrap().is_empty());
            no_remove(&rec);
        }
    }
    #[tokio::test]
    async fn uncordon_owner_mismatch_clears_only_our_annotation() {
        for code in [tonic::Code::FailedPrecondition, tonic::Code::NotFound] {
            let (rec, mut spec, pods) = fixture();
            spec.autoscaling = None;
            seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
            *rec.uncordon_result.lock().unwrap() = Some(code);
            assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
            assert_eq!(count(&rec, "clear_victim"), 1);
            no_remove(&rec);
        }
    }
    #[tokio::test]
    async fn manual_coordinator_cordon_is_never_released_or_picked() {
        let (rec, spec, pods) = fixture();
        rec.hosts.lock().unwrap()[0].cordoned = true;
        tick(&rec, &spec, &pods, &[]).await;
        assert!(rec.log().is_empty());
    }
    #[tokio::test]
    async fn pinned_victim_beyond_budget_is_released_not_stranded() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        seed(&rec, "b", VictimKind::Shed, VictimPhase::Retiring);
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        let victims = rec.victims.lock().unwrap();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].node, "a");
        assert_eq!(count(&rec, "uncordon"), 1);
        no_remove(&rec);
    }
    #[tokio::test]
    async fn scale_down_off_mid_wave_releases_victims() {
        let (rec, mut spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        spec.autoscaling.as_mut().unwrap().scale_down = crate::scaler::ScaleDownMode::Off;
        assert!(!tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "uncordon"), 1);
        no_remove(&rec);
    }
    #[tokio::test]
    async fn autoscaling_removed_mid_wave_releases_victims() {
        let (rec, mut spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        seed(&rec, "b", VictimKind::Shed, VictimPhase::Removing);
        spec.autoscaling = None;
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "uncordon"), 1);
        assert_eq!(count(&rec, "remove_node b"), 1);
        // The removing victim stays until its Node goes.
        assert_eq!(rec.victims.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn stuck_roll_without_autoscaling_still_blocks_rolls() {
        let (rec, mut spec, pods) = fixture();
        spec.autoscaling = None;
        assert!(tick(&rec, &spec, &pods, &["c".into()]).await.blocks_roll);
        no_remove(&rec);
        assert_eq!(count(&rec, "mark_victim"), 0);
    }
    #[tokio::test]
    async fn roll_stuck_node_becomes_repair_victim_after_capacity_recovers() {
        let (rec, spec, pods) = fixture();
        script(&rec, "c", vec![Retirement::Granted]);
        assert!(tick(&rec, &spec, &pods, &["c".into()]).await.blocks_roll);
        let log = rec.log();
        assert_eq!(log[0], "mark_victim c Retiring");
        assert!(log[1].ends_with("repair"));
        assert_eq!(log[2], "mark_victim c Removing");
        assert_eq!(log[3], "remove_node c");
        assert_eq!(log.len(), 4, "DeleteHost waits for the Node to go");
    }
    #[tokio::test]
    async fn roll_stuck_repair_holds_under_pressure_but_is_not_released() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "c", VictimKind::Repair, VictimPhase::Retiring);
        rec.demand.lock().unwrap().queued_sessions = 1;
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "retire_host"), 1);
        assert_eq!(count(&rec, "uncordon"), 0);
        no_remove(&rec);
    }
    #[tokio::test]
    async fn in_flight_victim_blocks_the_roll() {
        let (rec, spec, pods) = fixture();
        script(&rec, "a", vec![Retirement::Granted]);
        *rec.fail_remove.lock().unwrap() = true;
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(
            rec.victims.lock().unwrap()[0].record.phase,
            VictimPhase::Removing
        );
        assert_eq!(count(&rec, "delete_host"), 0);
    }
    #[tokio::test]
    async fn granted_does_not_read_demand_again_or_release() {
        let (rec, spec, pods) = fixture();
        let initial = *rec.demand.lock().unwrap();
        *rec.demands.lock().unwrap() = [
            initial,
            FleetDemand {
                queued_sessions: 10,
                ..initial
            },
        ]
        .into();
        script(&rec, "a", vec![Retirement::Granted]);
        tick(&rec, &spec, &pods, &[]).await;
        assert_eq!(rec.demands.lock().unwrap().len(), 1);
        assert_eq!(count(&rec, "uncordon"), 0);
        assert_eq!(count(&rec, "remove_node"), 1);
    }

    #[tokio::test]
    async fn failed_removing_patch_never_reaches_cloud() {
        let (rec, spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        script(&rec, "a", vec![Retirement::Granted]);
        *rec.mark_victim_fails.lock().unwrap() = true;
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        no_remove(&rec);
        assert_eq!(
            rec.victims.lock().unwrap()[0].record.phase,
            VictimPhase::Retiring
        );
    }

    #[tokio::test]
    async fn observe_only_keeps_leftover_victims_unchanged() {
        let (rec, mut spec, pods) = fixture();
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Removing);
        seed(&rec, "b", VictimKind::Repair, VictimPhase::Retiring);
        spec.autoscaling = None;
        step(
            &spec,
            None,
            &ScaleDownHysteresis::default(),
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: true,
                stuck_rolls: &[],
            },
        )
        .await
        .unwrap();
        assert!(rec.log().is_empty());
        assert_eq!(rec.victims.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn every_pending_victim_gets_one_retire_and_one_demand_read_per_tick() {
        let (mut rec, mut spec, mut pods) = fixture();
        Arc::get_mut(&mut rec).unwrap().record_reads = true;
        spec.autoscaling.as_mut().unwrap().max_shed_per_wave = 2;
        pods.push(ready_pod("d"));
        let mut extra = rec.hosts.lock().unwrap()[0].clone();
        extra.id = HostId::from_node_name("d");
        rec.hosts.lock().unwrap().push(extra);
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        seed(&rec, "b", VictimKind::Shed, VictimPhase::Retiring);
        assert!(tick(&rec, &spec, &pods, &[]).await.blocks_roll);
        assert_eq!(count(&rec, "retire_host"), 2);
        assert_eq!(count(&rec, "fleet_demand"), 3);
        assert_eq!(count(&rec, "list_hosts"), 1);
        no_remove(&rec);
    }
    #[tokio::test]
    async fn invalid_victim_annotation_is_read_only() {
        let mut node = Node::default();
        node.metadata.name = Some("invalid".into());
        node.metadata.annotations = Some([(VICTIM_ANNOTATION.into(), "garbage".into())].into());
        let snapshot = [node];
        // Build the lazy HTTP client only. These operations must not send a request.
        let client =
            Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
        let nodes = K8sNodeOps {
            client,
            managed_nodes: &snapshot,
        };
        assert!(nodes.victims("kvm").await.unwrap().is_empty());
        assert!(nodes.mark_victim("invalid", &retiring()).await.is_err());
        assert_eq!(
            snapshot[0].metadata.annotations.as_ref().unwrap()[VICTIM_ANNOTATION],
            "garbage"
        );
    }

    #[tokio::test]
    async fn repair_victims_run_first_and_share_the_in_flight_bound() {
        let (rec, mut spec, mut pods) = fixture();
        spec.autoscaling.as_mut().unwrap().max_shed_per_wave = 2;
        pods.push(ready_pod("d"));
        let mut extra = rec.hosts.lock().unwrap()[0].clone();
        extra.id = HostId::from_node_name("d");
        rec.hosts.lock().unwrap().push(extra);
        seed(&rec, "a", VictimKind::Shed, VictimPhase::Retiring);
        seed(&rec, "b", VictimKind::Repair, VictimPhase::Retiring);
        assert!(tick(&rec, &spec, &pods, &["c".into()]).await.blocks_roll);
        assert_eq!(rec.victims.lock().unwrap().len(), 2);
        assert!(rec.log()[0].ends_with("repair"));
        assert_eq!(count(&rec, "mark_victim"), 0);
        assert_eq!(count(&rec, "retire_host"), 2);
    }
}
