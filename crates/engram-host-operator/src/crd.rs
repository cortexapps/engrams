//! ADR 0044 K3: the `HostFleet` custom resource.
//!
//! A `HostFleet` declares the target host-agent + node-assets images and the
//! rollout guardrails. The operator patches the (chart-owned) host-agent
//! DaemonSet's images toward this CR, then rolls each node's pod one at a
//! time. Image rolls preserve the node's VMs and respect the capacity floor.
//! Cloud removal requires a coordinator retirement grant.
//!
//! The chart still owns the DaemonSet's full pod spec (init container,
//! emptyDir, securityContext, …); the operator only drives the *image* and
//! the *image roll*, which is the part `OnDelete` deliberately leaves
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

    /// Coordinator gRPC endpoint (e.g. `http://engram-coordinator:50061`).
    /// The operator calls FleetService over gRPC for image rolls and retirement.
    pub coordinator_url: String,

    /// Never drain a node if doing so would drop the fleet below this many
    /// schedulable (Ready) hosts. Draining evacuates live sessions onto
    /// peers, so there must always be receiving capacity.
    #[serde(default)]
    pub capacity_floor: u32,

    /// Shed retirement deadline in seconds. Also bounds the image roll
    /// successor gate and contributes to the handoff TTL.
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_seconds: u64,

    /// ADR 0088: seconds the image roll waits (post-cordon, pre-pod-delete)
    /// for the host's in-flight enable work — a live materialize or a
    /// base-snapshot capture — to finish before killing the host-agent pod.
    /// The cordon stops NEW work arriving, so this waits out at most the
    /// tail of what is already running (a dev-brain materialize ~55-90 min;
    /// a capture = its warm timeout + freeze). On timeout the roll proceeds
    /// LOUDLY (the enable's durable-job retry remains the backstop — the
    /// pre-0088 behavior, demoted to rare fallback). `0` disables the gate.
    #[serde(default = "default_enable_work_timeout")]
    pub enable_work_timeout_seconds: u64,

    /// Optional node-pool autoscaling. The operator uses fleet demand to
    /// keep `targetFreeMib` of headroom. Explicit `noop` selection computes
    /// and logs the plan without autoscale mutations. GKE or ASG
    /// initialization errors fail operator startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscaling: Option<AutoscalingSpec>,
}

fn default_drain_timeout() -> u64 {
    600
}

fn default_enable_work_timeout() -> u64 {
    5400
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
    /// teleport). Node removal requires the coordinator retirement grant.
    #[serde(default)]
    pub scale_down: crate::scaler::ScaleDownMode,
    /// ADR 0045 Phase E: shed only after the scale-down decision holds for
    /// this many minute-sized ticks (anti-flap hysteresis — scale up fast,
    /// scale down slow). The time-sized tick keeps this stable when the
    /// scale-up reconcile interval changes.
    #[serde(default = "default_scale_down_hysteresis")]
    pub scale_down_hysteresis_ticks: u32,
    /// The most victim nodes retiring at once (ADR 0123 E3).
    #[serde(default = "default_max_shed_per_wave")]
    pub max_shed_per_wave: u32,
}

fn default_scale_down_hysteresis() -> u32 {
    3
}

fn default_max_shed_per_wave() -> u32 {
    1
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
