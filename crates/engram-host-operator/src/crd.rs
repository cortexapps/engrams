//! ADR 0044 K3: the `HostFleet` custom resource.
//!
//! A `HostFleet` declares the target host-agent + node-assets images and the
//! rollout guardrails. The operator patches the (chart-owned) host-agent
//! DaemonSet's images toward this CR, then rolls each node's pod one at a
//! time — draining the node's coordinator host (snapshot + warm-restore on a
//! peer) before deleting the pod, never dropping below the capacity floor.
//!
//! The chart still owns the DaemonSet's full pod spec (init container,
//! emptyDir, securityContext, …); the operator only drives the *image* and
//! the *drain-gated roll*, which is the part `OnDelete` deliberately leaves
//! to a controller.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "engram.io",
    version = "v1alpha1",
    kind = "HostFleet",
    namespaced,
    status = "HostFleetStatus",
    shortname = "hf",
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".spec.image"}"#,
    printcolumn = r#"{"name":"Floor","type":"integer","jsonPath":".spec.capacityFloor"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct HostFleetSpec {
    /// Namespace + name of the host-agent DaemonSet this fleet manages.
    pub daemon_set: DaemonSetRef,

    /// Target host-agent container image (digest-pinned in prod). A change
    /// here is what the operator rolls — never a registry-tag move.
    pub image: String,

    /// Target node-assets image (the `stage-node-assets` init container,
    /// ADR 0044 GAP 2). Rolled together with `image`: both ride the pod
    /// (per-pod emptyDir), so there's no swap under a live VM.
    pub node_assets_image: String,

    /// Base URL of the coordinator (e.g. `http://engram-coordinator:8080`).
    /// The operator drives cordon/drain on `/api/admin/...` and polls the
    /// drain gate on `/api/hosts/:id`.
    pub coordinator_url: String,

    /// Never drain a node if doing so would drop the fleet below this many
    /// schedulable (Ready) hosts. Draining evacuates live sessions onto
    /// peers, so there must always be receiving capacity.
    #[serde(default)]
    pub capacity_floor: u32,

    /// Seconds to wait for a drained host's `running_sandboxes` to reach 0
    /// before aborting the roll (leaving the pod in place).
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_seconds: u64,

    /// ADR 0044 K4: optional node-pool autoscaling. When set, the operator
    /// polls the coordinator's fleet demand and resizes the host node pool to
    /// hold `targetFreeMib` of headroom (scale-up only in v1; drain-gated
    /// scale-down is a follow-up). With the default noop scaler the operator
    /// only *logs* the desired size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscaling: Option<AutoscalingSpec>,
}

fn default_drain_timeout() -> u64 {
    600
}

/// Reference to the host-agent DaemonSet the operator patches + rolls.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSetRef {
    pub namespace: String,
    pub name: String,
}

/// ADR 0044 K4: node-pool autoscaling config.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoscalingSpec {
    /// The cloud node pool the scaler resizes (actuator-specific identifier,
    /// e.g. a GKE node-pool name).
    pub node_pool: String,
    /// Never size the pool below this (set ≥ 1 — an empty fleet has no
    /// demand signal to grow from).
    pub min_hosts: u32,
    pub max_hosts: u32,
    /// Keep at least this much free guest-RAM (MiB) across the fleet.
    pub target_free_mib: u64,
    /// ADR 0045 Phase E: whether the pool may shrink, and how aggressively.
    /// `off` (default) is K4's scale-up-only behavior. `idleOnly` sheds only a
    /// fully-idle host; `aggressive` sheds the least-loaded host (sessions
    /// evacuate). The scale-down *decision* is computed + logged today; live
    /// node removal is the follow-up actuator (safe node-specific removal).
    #[serde(default)]
    pub scale_down: crate::scaler::ScaleDownMode,
    /// ADR 0045 Phase E: shed only after the scale-down decision holds for
    /// this many consecutive reconciles (anti-flap hysteresis — scale up fast,
    /// scale down slow). Pairs with the cold-node cost of re-adding a node.
    #[serde(default = "default_scale_down_hysteresis")]
    pub scale_down_hysteresis_ticks: u32,
}

fn default_scale_down_hysteresis() -> u32 {
    3
}

/// Reported progress. v1 leaves this for `kubectl` visibility; the operator
/// logs decisions and (future) writes the roll state here for a status-driven
/// state machine.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostFleetStatus {
    /// Pods already on the target images.
    pub rolled_nodes: i32,
    /// Total host-agent pods observed.
    pub total_nodes: i32,
    /// The node currently being rolled, if any.
    pub rolling_node: Option<String>,
    /// Last action / blocker, human-readable.
    pub message: String,
}
