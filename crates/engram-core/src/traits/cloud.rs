use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;

use crate::error::BackendError;
use crate::types::host::{HostMetadata, HostSpec, PreemptionNotice};
use crate::types::ids::HostId;

pub type PreemptionStream = Pin<Box<dyn Stream<Item = PreemptionNotice> + Send + 'static>>;

/// Cloud-provider seam. Implementations: `engram-cloud-{gcp,static,mock}`,
/// future `engram-cloud-{aws,hetzner}`. See `DESIGN.md` for semantics.
#[async_trait]
pub trait CloudBackend: Send + Sync {
    /// Subscribe to preemption/eviction notices for the host this is
    /// running on. Returns a stream that typically emits a single event
    /// (cloud preemption is one-shot) before terminating.
    fn preemption_signal(&self) -> PreemptionStream;

    /// Get host metadata (instance ID, zone, machine type) for
    /// self-identification at startup.
    async fn host_metadata(&self) -> Result<HostMetadata, BackendError>;

    /// Optional: provision a new host. Static-fleet backends return
    /// `BackendError::NotSupported`.
    async fn provision_host(&self, spec: HostSpec) -> Result<HostId, BackendError>;

    /// Optional: tear down a previously-provisioned host.
    async fn deprovision_host(&self, id: HostId) -> Result<(), BackendError>;
}

/// ADR 0044 K4: node-pool autoscaling seam — declarative "make the host node
/// pool N nodes". Distinct from [`CloudBackend`]'s per-host provision model:
/// managed K8s node pools (GKE/EKS/AKS) resize by *count*, not by individual
/// host. The rollout operator owns the *policy* (how many nodes from demand);
/// an implementation owns the *actuation*. New clouds plug in by implementing
/// this in their own crate (`engram-cloud-gcp` does GKE today) — no change to
/// the operator beyond selecting the impl.
#[async_trait]
pub trait NodePoolScaler: Send + Sync {
    /// Set the node pool's desired size. Idempotent — re-asserted every
    /// reconcile. `node_pool` is an actuator-specific identifier.
    ///
    /// ADR 0048 invariant: `set_size` is **GROW-ONLY**. The caller must never
    /// pass a `desired` below the pool's current physical size — a count-only
    /// shrink lets the managed instance group pick an arbitrary (possibly
    /// loaded) victim. Shrinking goes through [`Self::remove_node`], which
    /// names the node. (A failed roll that leaves a host cordoned must not let
    /// a later `set_size(N-1)` on a physically-N pool delete a live node.)
    async fn set_size(&self, node_pool: &str, desired: u32) -> Result<(), BackendError>;

    /// ADR 0048: remove ONE specific node from the pool, atomically
    /// decrementing the pool's target size so the managed instance group
    /// doesn't recreate it. This is the ONLY shrink path (see the grow-only
    /// note on [`Self::set_size`]): naming the victim caps the blast radius at
    /// "this node", where a count-only shrink would delete whichever node the
    /// cloud chose. Idempotent — a node already gone (or never in the pool)
    /// returns `Ok(())`. `node_name` is the cloud's node identifier (the K8s
    /// Node name, which equals the GCE instance name on GKE).
    async fn remove_node(&self, node_pool: &str, node_name: &str) -> Result<(), BackendError>;
}
