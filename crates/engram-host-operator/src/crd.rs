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
