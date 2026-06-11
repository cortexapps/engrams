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
    async fn set_size(&self, node_pool: &str, desired: u32) -> Result<(), BackendError>;
}
