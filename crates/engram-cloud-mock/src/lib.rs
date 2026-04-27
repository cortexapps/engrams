//! Mock CloudBackend for tests. Lets a test trigger a preemption notice
//! on demand and inspect provision/deprovision calls.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_core::traits::cloud::{CloudBackend, PreemptionStream};
use engram_core::types::host::{HostMetadata, HostSpec, PreemptionNotice};
use engram_core::{BackendError, HostId};
use parking_lot::Mutex;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct MockCloud {
    inner: Arc<MockInner>,
}

struct MockInner {
    metadata: HostMetadata,
    preempt_tx: broadcast::Sender<PreemptionNotice>,
    provisioned: Mutex<Vec<HostSpec>>,
    deprovisioned: Mutex<Vec<HostId>>,
}

impl MockCloud {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(8);
        Self {
            inner: Arc::new(MockInner {
                metadata: HostMetadata {
                    instance_id: "mock-instance".into(),
                    zone: "mock".into(),
                    machine_type: "mock".into(),
                    extra: serde_json::Value::Null,
                },
                preempt_tx: tx,
                provisioned: Mutex::new(Vec::new()),
                deprovisioned: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Send a preemption notice to all current subscribers.
    pub fn trigger_preemption(&self, reason: impl Into<String>, deadline_secs: Option<u32>) {
        let _ = self.inner.preempt_tx.send(PreemptionNotice {
            reason: reason.into(),
            deadline_secs,
            received_at: Utc::now(),
        });
    }

    pub fn provisioned_specs(&self) -> Vec<HostSpec> {
        self.inner.provisioned.lock().clone()
    }

    pub fn deprovisioned_ids(&self) -> Vec<HostId> {
        self.inner.deprovisioned.lock().clone()
    }
}

impl Default for MockCloud {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CloudBackend for MockCloud {
    fn preemption_signal(&self) -> PreemptionStream {
        let mut rx = self.inner.preempt_tx.subscribe();
        let stream = async_stream::stream! {
            while let Ok(notice) = rx.recv().await {
                yield notice;
            }
        };
        Box::pin(stream)
    }

    async fn host_metadata(&self) -> Result<HostMetadata, BackendError> {
        Ok(self.inner.metadata.clone())
    }

    async fn provision_host(&self, spec: HostSpec) -> Result<HostId, BackendError> {
        let id = HostId::new();
        self.inner.provisioned.lock().push(spec);
        Ok(id)
    }

    async fn deprovision_host(&self, id: HostId) -> Result<(), BackendError> {
        self.inner.deprovisioned.lock().push(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::HostSpec;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::time::timeout;

    fn spec() -> HostSpec {
        HostSpec {
            machine_type: "n2d-highmem-32".into(),
            zone: "us-central1-a".into(),
            preemptible: true,
            disk_gb: 200,
            labels: vec![("env".into(), "test".into())],
        }
    }

    #[tokio::test]
    async fn host_metadata_returns_mock_identity() {
        let cloud = MockCloud::new();
        let meta = cloud.host_metadata().await.unwrap();
        assert_eq!(meta.instance_id, "mock-instance");
        assert_eq!(meta.zone, "mock");
    }

    #[tokio::test]
    async fn provision_and_deprovision_are_recorded() {
        let cloud = MockCloud::new();
        let id = cloud.provision_host(spec()).await.unwrap();
        cloud.deprovision_host(id).await.unwrap();

        let provisioned = cloud.provisioned_specs();
        assert_eq!(provisioned.len(), 1);
        assert_eq!(provisioned[0].machine_type, "n2d-highmem-32");
        assert_eq!(provisioned[0].zone, "us-central1-a");
        assert!(provisioned[0].preemptible);

        assert_eq!(cloud.deprovisioned_ids(), vec![id]);
    }

    #[tokio::test]
    async fn preemption_signal_emits_after_trigger() {
        let cloud = MockCloud::new();
        let mut stream = cloud.preemption_signal();

        // Subscriber must be live before trigger fires (broadcast semantics).
        // Spawn the trigger after a short delay to verify the subscriber is
        // actually awaiting the stream rather than racing past it.
        let cloud_t = cloud.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cloud_t.trigger_preemption("simulated", Some(30));
        });

        let notice = timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("preemption notice arrived")
            .expect("stream did not end before notice");
        assert_eq!(notice.reason, "simulated");
        assert_eq!(notice.deadline_secs, Some(30));
    }

    #[tokio::test]
    async fn preemption_fans_out_to_multiple_subscribers() {
        let cloud = MockCloud::new();
        let mut s1 = cloud.preemption_signal();
        let mut s2 = cloud.preemption_signal();

        cloud.trigger_preemption("multi", None);

        let n1 = timeout(Duration::from_secs(2), s1.next())
            .await
            .unwrap()
            .unwrap();
        let n2 = timeout(Duration::from_secs(2), s2.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n1.reason, "multi");
        assert_eq!(n2.reason, "multi");
    }

    #[tokio::test]
    async fn preemption_signal_before_trigger_is_inert() {
        let cloud = MockCloud::new();
        let mut stream = cloud.preemption_signal();
        // No trigger fired — subscriber must not yield within a small window.
        let r = timeout(Duration::from_millis(50), stream.next()).await;
        assert!(r.is_err(), "stream must not yield without a trigger");
    }
}
