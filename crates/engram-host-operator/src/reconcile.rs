//! ADR 0044 K3 reconcile loop: drive a host-agent DaemonSet toward the
//! `HostFleet` CR, rolling one node at a time.
//!
//! Each reconcile: (1) patch the DaemonSet images toward the CR (so
//! successors come up on target — `OnDelete` leaves existing pods alone),
//! (2) list the pods, (3) [`plan_roll`] picks the next action, (4) if it's a
//! node roll, cordon → delete the pod → wait for the successor Ready on
//! target → uncordon. The roll **reattaches, it does not evacuate**: ADR 0044
//! K2 keeps the node's microVMs alive across the pod swap and the successor's
//! `reattach_pass` adopts them, so an image roll is lossless (no
//! snapshot-rehome rewind). Drain/evac is reserved for actual node removal
//! (see [`gate_drain`]).

use std::sync::atomic::AtomicU32;
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
    /// Roll this node next (delete its pod; the successor reattaches the
    /// node's still-running microVMs — no drain/evac).
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

    // ADR 0044 K4 + ADR 0048: autoscale the node pool from coordinator demand
    // (scale-up via set_size; scale-down via the teleport-packed wave). A fresh
    // wave only starts when the image roll is quiescent (`roll_idle`); an
    // in-flight wave blocks new rolls (returned in `wave_in_flight`). Best-
    // effort — a coord hiccup shouldn't wedge the controller.
    let roll_idle = matches!(decision, RollDecision::UpToDate { .. });
    let wave_in_flight = match run_autoscale(spec, &ctx, &pods, roll_idle).await {
        Ok(status) => status.wave_in_flight,
        Err(e) => {
            tracing::warn!(error = %e, "autoscale step failed; continuing");
            false
        }
    };

    // 4. Act. An in-flight scale-down wave takes precedence over image rolls
    //    (the wave's drains are consuming receiving capacity) — requeue soon.
    if wave_in_flight {
        return Ok(Action::requeue(Duration::from_secs(10)));
    }
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

/// One autoscale reconcile: builds the live coordinator + K8s seams and runs
/// [`crate::autoscale::step`]. No-op (returns "no wave") unless the CR sets
/// `autoscaling`.
async fn run_autoscale(
    spec: &HostFleetSpec,
    ctx: &Ctx,
    pods: &[PodInfo],
    roll_idle: bool,
) -> Result<crate::autoscale::AutoscaleStatus, OperatorError> {
    if spec.autoscaling.is_none() {
        return Ok(crate::autoscale::AutoscaleStatus::default());
    }
    let coord = CoordClient::new(spec.coordinator_url.clone(), coord_token());
    let nodes = crate::autoscale::K8sNodeOps {
        client: ctx.client.clone(),
    };
    crate::autoscale::step(
        spec,
        ctx.scaler.as_ref(),
        &ctx.scaledown_ticks,
        &coord,
        &nodes,
        pods,
        roll_idle,
    )
    .await
}

/// Roll one node's host-agent pod onto the target image **by reattach, not
/// evacuation**: cordon → delete the pod → wait for the successor to come up
/// Ready on target → uncordon.
///
/// ADR 0044 K2 keeps the node's microVMs alive across a pod restart
/// (`hostPID`/`hostNetwork`/cgroup-escape), and the successor's
/// `reattach_pass` pidfd-adopts them — so an image roll is **lossless**. We
/// deliberately do *not* drain/evacuate: snapshot-rehome evac rewinds the
/// session to the last periodic checkpoint (the spurious "N events rolled
/// back"), losing post-checkpoint work for no reason when the VM never died.
/// Drain/evac is reserved for actual **node removal** (scale-down, node
/// maintenance), where the VMs genuinely die with the node — see
/// [`gate_drain`].
///
/// The cordon stays in force across the swap so the dead-host detector (which
/// only strikes out `ready` hosts) skips this `draining` host during its brief
/// heartbeat gap — otherwise the gap could strike the host out and route its
/// (reattaching) sessions to Idle out from under the successor.
async fn roll_node(
    client: &Client,
    spec: &HostFleetSpec,
    node: &str,
    pod: &str,
) -> Result<(), OperatorError> {
    let host_id = HostId::from_node_name(node);
    let coord = CoordClient::new(spec.coordinator_url.clone(), coord_token());

    tracing::info!(%node, %host_id, %pod, "rolling node: cordon + reattach (no drain — the node's VMs survive)");
    // The load-bearing cordon is the coordinator's (stops new session
    // placement); the K8s node cordon stops any stray scheduling.
    set_node_unschedulable(client, node, true).await?;
    coord.cordon(host_id).await?;

    // Delete the pod. With `OnDelete` the DaemonSet recreates it on the target
    // image; the successor's `reattach_pass` adopts the still-running microVMs.
    let pods: Api<Pod> = Api::namespaced(client.clone(), &spec.daemon_set.namespace);
    pods.delete(pod, &DeleteParams::default()).await?;
    tracing::info!(%node, %pod, "pod deleted; waiting for the successor to come up Ready on target + reattach");

    // Gate on the successor being Ready on the target image BEFORE uncordoning
    // — keeping the host `draining` (dead-host-detector-exempt) through the
    // swap so the reattaching sessions are never struck out.
    gate_successor_ready(
        client,
        spec,
        node,
        Duration::from_secs(spec.drain_timeout_seconds),
    )
    .await?;

    // The successor re-registered under the same stable HostId (GAP 1) and is
    // Ready; resume scheduling.
    coord.uncordon(host_id).await?;
    set_node_unschedulable(client, node, false).await?;
    tracing::info!(%node, %host_id, "successor Ready + reattached; uncordoned");
    Ok(())
}

/// Poll the DaemonSet's pods until a Ready pod on `node` carries the target
/// images (the successor has come up + run its reattach pass), or the budget
/// elapses (the roll aborts with the host still cordoned; the next reconcile
/// retries).
/// The reattach gate's terminal condition: a Ready pod on `node` carrying
/// **both** target images (so its host-agent has come up and run its reattach
/// pass). Pure, so it's unit-tested without a cluster. A roll only uncordons
/// once this holds — keeping the host `draining` (dead-host-detector-exempt)
/// through the swap.
fn successor_ready(pods: &[PodInfo], node: &str, image: &str, node_assets_image: &str) -> bool {
    pods.iter()
        .any(|p| p.node == node && p.ready && p.on_target(image, node_assets_image))
}

async fn gate_successor_ready(
    client: &Client,
    spec: &HostFleetSpec,
    node: &str,
    budget: Duration,
) -> Result<(), OperatorError> {
    let ds_api: Api<DaemonSet> = Api::namespaced(client.clone(), &spec.daemon_set.namespace);
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let ds = ds_api.get(&spec.daemon_set.name).await?;
        let pods = list_ds_pods(client, &spec.daemon_set.namespace, &ds).await?;
        if successor_ready(&pods, node, &spec.image, &spec.node_assets_image) {
            tracing::info!(%node, "successor pod Ready on target");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(OperatorError::RollTimeout {
                node: node.to_string(),
            });
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
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

pub(crate) fn coord_token() -> Option<String> {
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

    // ADR 0044 K3 amendment: the reattach roll uncordons only once the
    // successor pod is Ready ON TARGET — that's what holds the host `draining`
    // (dead-host-detector-exempt) through the swap. Lock the gate condition.
    #[test]
    fn successor_ready_requires_a_ready_on_target_pod_on_the_node() {
        // The successor: Ready, both target images, right node → gate opens.
        let pods = vec![pod("a", NEW, NA, true)];
        assert!(successor_ready(&pods, "a", NEW, NA));

        // Not yet Ready (still coming up) → keep waiting.
        assert!(!successor_ready(&[pod("a", NEW, NA, false)], "a", NEW, NA));
        // Ready but still on the OLD host image → not the successor yet.
        assert!(!successor_ready(&[pod("a", OLD, NA, true)], "a", NEW, NA));
        // Ready on target but the node-assets image drifted → not on target.
        assert!(!successor_ready(
            &[pod("a", NEW, "ghcr/node-assets@sha256:OLD", true)],
            "a",
            NEW,
            NA
        ));
        // A Ready on-target pod, but on a DIFFERENT node → doesn't open this
        // node's gate.
        assert!(!successor_ready(&[pod("b", NEW, NA, true)], "a", NEW, NA));
    }
}
