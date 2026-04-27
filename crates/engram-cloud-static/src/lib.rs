//! Static-fleet cloud backend.
//!
//! Used for bare-metal, Hetzner, or any deployment where hosts are
//! provisioned out-of-band. Preemption never fires; provisioning errors
//! with `BackendError::NotSupported`.

use async_trait::async_trait;
use engram_core::traits::cloud::{CloudBackend, PreemptionStream};
use engram_core::types::host::{HostMetadata, HostSpec, PreemptionNotice};
use engram_core::{BackendError, HostId};

pub struct StaticCloud {
    hostname: String,
}

impl StaticCloud {
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
        }
    }

    pub fn detect() -> Result<Self, BackendError> {
        if let Ok(h) = std::env::var("HOSTNAME") {
            if !h.is_empty() {
                return Ok(Self::new(h));
            }
        }
        let out = std::process::Command::new("hostname")
            .output()
            .map_err(|e| BackendError::Config(format!("could not run `hostname`: {e}")))?;
        let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if h.is_empty() {
            return Err(BackendError::Config("hostname is empty".into()));
        }
        Ok(Self::new(h))
    }
}

#[async_trait]
impl CloudBackend for StaticCloud {
    fn preemption_signal(&self) -> PreemptionStream {
        // Static hosts are never preempted by the cloud — return an
        // immediately-finished stream.
        Box::pin(futures::stream::empty::<PreemptionNotice>())
    }

    async fn host_metadata(&self) -> Result<HostMetadata, BackendError> {
        Ok(HostMetadata {
            instance_id: self.hostname.clone(),
            zone: "static".to_string(),
            machine_type: "static".to_string(),
            extra: serde_json::Value::Null,
        })
    }

    async fn provision_host(&self, _spec: HostSpec) -> Result<HostId, BackendError> {
        Err(BackendError::NotSupported(
            "provision_host on static backend",
        ))
    }

    async fn deprovision_host(&self, _id: HostId) -> Result<(), BackendError> {
        Err(BackendError::NotSupported(
            "deprovision_host on static backend",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::HostSpec;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn host_metadata_returns_supplied_hostname() {
        let cloud = StaticCloud::new("box-42");
        let meta = cloud.host_metadata().await.unwrap();
        assert_eq!(meta.instance_id, "box-42");
        assert_eq!(meta.zone, "static");
        assert_eq!(meta.machine_type, "static");
    }

    #[tokio::test]
    async fn preemption_signal_is_immediately_finished() {
        let cloud = StaticCloud::new("box-42");
        let mut stream = cloud.preemption_signal();
        // The static backend never preempts: the stream must yield None
        // (terminate) rather than block. Bound it under a small timeout
        // so a regression that returns a hanging stream gets caught.
        let next = timeout(Duration::from_millis(50), stream.next())
            .await
            .expect("stream must terminate, not hang");
        assert!(
            next.is_none(),
            "static stream must end without yielding a notice"
        );
    }

    #[tokio::test]
    async fn provision_and_deprovision_are_unsupported() {
        let cloud = StaticCloud::new("box-42");
        let spec = HostSpec {
            machine_type: "x".into(),
            zone: "y".into(),
            preemptible: false,
            disk_gb: 0,
            labels: vec![],
        };
        match cloud.provision_host(spec).await {
            Err(BackendError::NotSupported(op)) => assert!(op.contains("provision")),
            other => panic!("expected NotSupported, got {other:?}"),
        }
        match cloud.deprovision_host(HostId::new()).await {
            Err(BackendError::NotSupported(op)) => assert!(op.contains("deprovision")),
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }
}
