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
