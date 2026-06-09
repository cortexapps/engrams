//! ADR 0044 K3 reconcile loop: drive a host-agent DaemonSet toward the
//! `HostFleet` CR, rolling one node at a time with a drain gate.
//!
//! Each reconcile: (1) patch the DaemonSet images toward the CR (so
//! successors come up on target — `OnDelete` leaves existing pods alone),
//! (2) list the pods, (3) [`plan_roll`] picks the next action, (4) if it's a
//! node roll, cordon → drain → gate on `running_sandboxes → 0` → delete the
//! pod → uncordon. The DaemonSet recreates the pod on the target image; the
//! next reconcile sees it and moves on once it's Ready.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engram_core::HostId;
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Client, ResourceExt};

use engram_core::traits::cloud::NodePoolScaler;

use crate::coord::CoordClient;
use crate::crd::{HostFleet, HostFleetSpec};
use crate::error::OperatorError;
use crate::scaler::{desired_hosts, AutoscalePolicy};

const HOST_AGENT_CONTAINER: &str = "host-agent";
const STAGE_ASSETS_CONTAINER: &str = "stage-node-assets";

/// Context shared across reconciles.
pub struct Ctx {
    pub client: Client,
    /// ADR 0044 K4: actuates node-pool size changes. `NoopScaler` by default.
    pub scaler: Arc<dyn NodePoolScaler>,
    /// ADR 0045 Phase E: consecutive reconciles the scale-down decision has
    /// held, for anti-flap hysteresis. Reset to 0 on any hold/scale-up tick.
    pub scaledown_ticks: AtomicU32,
}

/// A host-agent pod distilled to the fields the planner needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodInfo {
    pub name: String,
    pub node: String,
    pub host_image: String,
    pub init_image: String,
    pub ready: bool,
}

impl PodInfo {
    fn on_target(&self, image: &str, node_assets_image: &str) -> bool {
        self.host_image == image && self.init_image == node_assets_image
    }
}

/// The next action the planner picks. A pure function of the pods + spec, so
/// it's exhaustively unit-testable without a cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RollDecision {
    /// Every pod is already on the target images.
    UpToDate { total: usize },
    /// A pod is mid-restart (not Ready) — let the fleet settle before
    /// starting another roll.
    WaitForReady,
    /// Rolling the next stale node would drop below the capacity floor.
    BlockedByFloor { schedulable: usize, floor: u32 },
    /// Roll this node next (drain its host, delete its pod).
    RollNode { node: String, pod: String },
}

/// Pure roll planner. Rolls stale pods oldest-by-node-name first (stable
/// across reconciles + tests). Only starts a roll when the whole fleet is
/// Ready (no in-flight restart) and the floor still holds after losing one
/// host.
pub fn plan_roll(
    pods: &[PodInfo],
    image: &str,
    node_assets_image: &str,
    floor: u32,
) -> RollDecision {
    let total = pods.len();
    let mut stale: Vec<&PodInfo> = pods
        .iter()
        .filter(|p| !p.on_target(image, node_assets_image))
        .collect();
    if stale.is_empty() {
        return RollDecision::UpToDate { total };
    }
    // Don't start a new roll while any pod is still coming up — that pod may
    // be the successor of the node we just rolled.
    if pods.iter().any(|p| !p.ready) {
        return RollDecision::WaitForReady;
    }
    let schedulable = pods.iter().filter(|p| p.ready).count();
    if (schedulable as u32).saturating_sub(1) < floor {
        return RollDecision::BlockedByFloor { schedulable, floor };
    }
    stale.sort_by(|a, b| a.node.cmp(&b.node));
    let next = stale[0];
    RollDecision::RollNode {
        node: next.node.clone(),
        pod: next.name.clone(),
    }
}

/// The controller entry point.
pub async fn reconcile(hf: Arc<HostFleet>, ctx: Arc<Ctx>) -> Result<Action, OperatorError> {
    let spec = &hf.spec;
    let client = &ctx.client;
    let ds_api: Api<DaemonSet> = Api::namespaced(client.clone(), &spec.daemon_set.namespace);
    let ds = ds_api.get(&spec.daemon_set.name).await?;

    // 1. Reconcile the DaemonSet images toward the CR. OnDelete → existing
    //    pods are untouched; only successors pick the new images up.
    ensure_ds_images(&ds_api, &ds, &spec.image, &spec.node_assets_image).await?;

    // 2. List the DaemonSet's pods.
    let pods = list_ds_pods(client, &spec.daemon_set.namespace, &ds).await?;

    // 3. Plan.
    let decision = plan_roll(
        &pods,
        &spec.image,
        &spec.node_assets_image,
        spec.capacity_floor,
    );
    tracing::info!(fleet = %hf.name_any(), ?decision, "reconcile");

    // ADR 0044 K4: autoscale the node pool from coordinator demand. Best-
    // effort + independent of the roll — a coord hiccup shouldn't block rolls.
    if let Err(e) = maybe_autoscale(spec, &ctx).await {
        tracing::warn!(error = %e, "autoscale step failed; continuing");
    }

    // 4. Act.
    match decision {
        RollDecision::UpToDate { .. } => Ok(Action::requeue(Duration::from_secs(60))),
        RollDecision::WaitForReady => Ok(Action::requeue(Duration::from_secs(10))),
        RollDecision::BlockedByFloor {
            schedulable, floor, ..
        } => {
            tracing::warn!(
                schedulable,
                floor,
                "roll blocked by capacity floor; need more receiving capacity"
            );
            Ok(Action::requeue(Duration::from_secs(30)))
        }
        RollDecision::RollNode { node, pod } => {
            roll_node(client, spec, &node, &pod).await?;
            Ok(Action::requeue(Duration::from_secs(5)))
        }
    }
}

/// ADR 0044 K4 + ADR 0045 Phase E: read coordinator fleet demand and drive
/// the node pool toward the headroom target. No-op unless the CR sets
/// `autoscaling`.
///
/// **Scale-up / hold** actuate immediately via `set_size` (idempotent
/// re-assert), and reset the scale-down hysteresis counter.
///
/// **Scale-down** is *never* actuated through `set_size` — that's count-only,
/// so the cloud could remove a node with live microVMs. Instead the decision
/// is hysteresis-gated and, once confirmed, **logged** (the safe live
/// actuation — drain the specific least-loaded node, then remove *that* node
/// via a node-specific cloud call — is the follow-up actuator, ADR 0045 Phase
/// E). This mirrors how K4 scale-up first shipped behind the logging
/// `NoopScaler`.
async fn maybe_autoscale(spec: &HostFleetSpec, ctx: &Ctx) -> Result<(), OperatorError> {
    let Some(a) = &spec.autoscaling else {
        return Ok(());
    };
    let coord = CoordClient::new(spec.coordinator_url.clone(), coord_token());
    let demand = coord.fleet_demand().await?;
    let current = demand.schedulable_hosts;
    let policy = AutoscalePolicy {
        min_hosts: a.min_hosts,
        max_hosts: a.max_hosts,
        target_free_mib: a.target_free_mib,
        scale_down: a.scale_down,
    };
    let desired = desired_hosts(demand, policy);

    if desired < current {
        // Scale-down pressure. Hold the hysteresis counter and only surface a
        // CONFIRMED decision after it persists — never call set_size here.
        let ticks = ctx.scaledown_ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let required = a.scale_down_hysteresis_ticks.max(1);
        if ticks >= required {
            tracing::info!(
                node_pool = %a.node_pool,
                current,
                desired,
                free_mib = demand.free_mib,
                mode = ?a.scale_down,
                "autoscale: scale-down CONFIRMED — would drain the least-loaded \
                 host and remove its node (live node removal pending the \
                 node-specific actuator; ADR 0045 Phase E)"
            );
        } else {
            tracing::info!(
                node_pool = %a.node_pool,
                current,
                desired,
                ticks,
                required,
                "autoscale: scale-down candidate, holding for hysteresis"
            );
        }
        return Ok(());
    }

    // Scale-up or hold: actuate (idempotent) and clear any scale-down streak.
    ctx.scaledown_ticks.store(0, Ordering::Relaxed);
    tracing::info!(
        node_pool = %a.node_pool,
        current,
        free_mib = demand.free_mib,
        desired,
        "autoscale: computed desired host count"
    );
    ctx.scaler.set_size(&a.node_pool, desired).await?;
    Ok(())
}

/// Cordon → drain → gate → delete the pod → uncordon, for one node.
async fn roll_node(
    client: &Client,
    spec: &HostFleetSpec,
    node: &str,
    pod: &str,
) -> Result<(), OperatorError> {
    let host_id = HostId::from_node_name(node);
    let coord = CoordClient::new(spec.coordinator_url.clone(), coord_token());

    tracing::info!(%node, %host_id, %pod, "rolling node: cordon + drain");
    // The K8s node cordon is best-effort — DaemonSet pods ignore it (so the
    // successor still comes back), but it stops any stray scheduling. The
    // load-bearing cordon is the coordinator's (stops session placement).
    set_node_unschedulable(client, node, true).await?;
    coord.cordon(host_id).await?;
    coord.drain(host_id).await?;

    gate_drain(
        &coord,
        host_id,
        Duration::from_secs(spec.drain_timeout_seconds),
    )
    .await?;

    let pods: Api<Pod> = Api::namespaced(client.clone(), &spec.daemon_set.namespace);
    pods.delete(pod, &DeleteParams::default()).await?;
    tracing::info!(%node, %pod, "drained + pod deleted; DaemonSet will roll the successor onto target");

    // The successor re-registers under the same stable HostId (GAP 1).
    coord.uncordon(host_id).await?;
    set_node_unschedulable(client, node, false).await?;
    Ok(())
}

/// Poll the coordinator's drain gate until the host reports
/// `running_sandboxes == 0`, or the budget elapses.
async fn gate_drain(
    coord: &CoordClient,
    host: HostId,
    budget: Duration,
) -> Result<(), OperatorError> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match coord.host_status(host).await? {
            // Deregistered — nothing left on this host.
            None => return Ok(()),
            Some(st) if st.running_sandboxes == 0 => {
                tracing::info!(%host, "drain complete (0 running sandboxes)");
                return Ok(());
            }
            Some(st) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(OperatorError::DrainTimeout {
                        host_id: host.to_string(),
                        remaining: st.running_sandboxes,
                    });
                }
                tracing::info!(%host, running = st.running_sandboxes, "draining…");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Strategic-merge-patch the DaemonSet so the host-agent container + the
/// node-assets init container carry the target images. The merge key is the
/// container `name`, so the rest of the pod spec is untouched.
async fn ensure_ds_images(
    api: &Api<DaemonSet>,
    ds: &DaemonSet,
    image: &str,
    node_assets_image: &str,
) -> Result<(), OperatorError> {
    let tmpl = ds.spec.as_ref().and_then(|s| s.template.spec.as_ref());
    let host_cur = tmpl
        .and_then(|s| s.containers.iter().find(|c| c.name == HOST_AGENT_CONTAINER))
        .and_then(|c| c.image.as_deref());
    let init_cur = tmpl
        .and_then(|s| s.init_containers.as_ref())
        .and_then(|ics| ics.iter().find(|c| c.name == STAGE_ASSETS_CONTAINER))
        .and_then(|c| c.image.as_deref());

    if host_cur == Some(image) && init_cur == Some(node_assets_image) {
        return Ok(());
    }

    tracing::info!(
        ds = %ds.name_any(),
        %image,
        %node_assets_image,
        "patching DaemonSet images toward HostFleet target"
    );
    let patch = serde_json::json!({
        "spec": { "template": { "spec": {
            "containers": [{ "name": HOST_AGENT_CONTAINER, "image": image }],
            "initContainers": [{ "name": STAGE_ASSETS_CONTAINER, "image": node_assets_image }],
        }}}
    });
    api.patch(
        &ds.name_any(),
        &PatchParams::default(),
        &Patch::Strategic(patch),
    )
    .await?;
    Ok(())
}

/// List the DaemonSet's pods (via its `matchLabels`) as [`PodInfo`].
async fn list_ds_pods(
    client: &Client,
    namespace: &str,
    ds: &DaemonSet,
) -> Result<Vec<PodInfo>, OperatorError> {
    let selector = ds
        .spec
        .as_ref()
        .and_then(|s| s.selector.match_labels.as_ref())
        .map(|ml| {
            ml.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .ok_or_else(|| OperatorError::Invalid("DaemonSet has no matchLabels selector".into()))?;

    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let list = pods.list(&ListParams::default().labels(&selector)).await?;
    Ok(list.into_iter().map(pod_info).collect())
}

fn pod_info(p: Pod) -> PodInfo {
    let name = p.name_any();
    let node = p
        .spec
        .as_ref()
        .and_then(|s| s.node_name.clone())
        .unwrap_or_default();
    let host_image = p
        .spec
        .as_ref()
        .and_then(|s| {
            s.containers
                .iter()
                .find(|c| c.name == HOST_AGENT_CONTAINER)
                .and_then(|c| c.image.clone())
        })
        .unwrap_or_default();
    let init_image = p
        .spec
        .as_ref()
        .and_then(|s| s.init_containers.as_ref())
        .and_then(|ics| {
            ics.iter()
                .find(|c| c.name == STAGE_ASSETS_CONTAINER)
                .and_then(|c| c.image.clone())
        })
        .unwrap_or_default();
    PodInfo {
        name,
        node,
        host_image,
        init_image,
        ready: pod_ready(&p),
    }
}

/// True iff the pod's `Ready` condition is `True`.
fn pod_ready(p: &Pod) -> bool {
    p.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
        .unwrap_or(false)
}

/// Patch a Node's `spec.unschedulable`.
async fn set_node_unschedulable(
    client: &Client,
    node: &str,
    val: bool,
) -> Result<(), OperatorError> {
    let nodes: Api<Node> = Api::all(client.clone());
    let patch = serde_json::json!({ "spec": { "unschedulable": val } });
    nodes
        .patch(node, &PatchParams::default(), &Patch::Merge(patch))
        .await?;
    Ok(())
}

fn coord_token() -> Option<String> {
    std::env::var("ENGRAM_COORDINATOR_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(node: &str, host_image: &str, init_image: &str, ready: bool) -> PodInfo {
        PodInfo {
            name: format!("hf-host-agent-{node}"),
            node: node.into(),
            host_image: host_image.into(),
            init_image: init_image.into(),
            ready,
        }
    }

    const NEW: &str = "ghcr/host-agent@sha256:new";
    const OLD: &str = "ghcr/host-agent@sha256:old";
    const NA: &str = "ghcr/node-assets@sha256:na";

    #[test]
    fn up_to_date_when_all_on_target() {
        let pods = vec![pod("a", NEW, NA, true), pod("b", NEW, NA, true)];
        assert_eq!(
            plan_roll(&pods, NEW, NA, 0),
            RollDecision::UpToDate { total: 2 }
        );
    }

    #[test]
    fn rolls_oldest_stale_node_first() {
        // Both stale + ready; floor 0. Picks the lexicographically-first node.
        let pods = vec![pod("b", OLD, NA, true), pod("a", OLD, NA, true)];
        assert_eq!(
            plan_roll(&pods, NEW, NA, 0),
            RollDecision::RollNode {
                node: "a".into(),
                pod: "hf-host-agent-a".into()
            }
        );
    }

    #[test]
    fn a_stale_node_asset_image_alone_triggers_a_roll() {
        // host image is current but the init (node-assets) image drifted.
        let pods = vec![pod("a", NEW, "ghcr/node-assets@sha256:OLD", true)];
        assert_eq!(
            plan_roll(&pods, NEW, NA, 0),
            RollDecision::RollNode {
                node: "a".into(),
                pod: "hf-host-agent-a".into()
            }
        );
    }

    #[test]
    fn waits_while_a_pod_is_not_ready() {
        // One stale-but-not-ready (a successor still coming up) blocks a new roll.
        let pods = vec![pod("a", NEW, NA, false), pod("b", OLD, NA, true)];
        assert_eq!(plan_roll(&pods, NEW, NA, 0), RollDecision::WaitForReady);
    }

    #[test]
    fn floor_blocks_the_last_host() {
        // Single stale host, floor 1: draining it would leave 0 < 1.
        let pods = vec![pod("a", OLD, NA, true)];
        assert_eq!(
            plan_roll(&pods, NEW, NA, 1),
            RollDecision::BlockedByFloor {
                schedulable: 1,
                floor: 1
            }
        );
    }

    #[test]
    fn floor_allows_a_roll_with_spare_capacity() {
        // Two ready hosts, floor 1: draining one leaves 1 >= 1 → roll.
        let pods = vec![pod("a", OLD, NA, true), pod("b", OLD, NA, true)];
        assert!(matches!(
            plan_roll(&pods, NEW, NA, 1),
            RollDecision::RollNode { .. }
        ));
    }
}
