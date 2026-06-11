//! GCP CloudBackend.
//!
//! Polls the GCE metadata server (`metadata.google.internal`) for
//! preemption notices on `instance/preempted` and `instance/maintenance-event`.
//! Phase 4 lights this up; Phase 5 wires the drain handler.

pub mod gke;

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use engram_core::traits::cloud::{CloudBackend, PreemptionStream};
use engram_core::types::host::{HostMetadata, HostSpec, PreemptionNotice};
use engram_core::{BackendError, HostId};
use serde::{Deserialize, Serialize};

const METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";

#[derive(Clone)]
pub struct GcpCloud {
    http: reqwest::Client,
    poll_interval: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstanceInfo {
    name: String,
    zone: String,
    #[serde(rename = "machineType")]
    machine_type: String,
}

impl GcpCloud {
    pub fn new() -> Result<Self, BackendError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        Ok(Self {
            http,
            poll_interval: Duration::from_secs(2),
        })
    }

    pub fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    async fn fetch(&self, path: &str) -> Result<String, BackendError> {
        let url = format!("{METADATA_BASE}/{path}");
        let resp = self
            .http
            .get(url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        if !resp.status().is_success() {
            return Err(BackendError::Protocol(format!(
                "metadata server returned {} for {path}",
                resp.status()
            )));
        }
        resp.text()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))
    }
}

#[async_trait]
impl CloudBackend for GcpCloud {
    fn preemption_signal(&self) -> PreemptionStream {
        let this = self.clone();
        let stream = async_stream::stream! {
            // Poll the maintenance-event endpoint. When the value flips
            // to TERMINATE_ON_HOST_MAINTENANCE we yield a notice and end.
            loop {
                match this.fetch("instance/maintenance-event").await {
                    Ok(v) if v.contains("TERMINATE") => {
                        yield PreemptionNotice {
                            reason: format!("gce maintenance: {v}"),
                            deadline_secs: Some(30),
                            received_at: Utc::now(),
                        };
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("gcp metadata poll error: {e}");
                    }
                }
                tokio::time::sleep(this.poll_interval).await;
            }
        };
        Box::pin(stream)
    }

    async fn host_metadata(&self) -> Result<HostMetadata, BackendError> {
        let name = self.fetch("instance/name").await.unwrap_or_default();
        let zone = self.fetch("instance/zone").await.unwrap_or_default();
        let machine_type = self
            .fetch("instance/machine-type")
            .await
            .unwrap_or_default();
        let info = InstanceInfo {
            name: name.clone(),
            zone: zone.clone(),
            machine_type: machine_type.clone(),
        };
        Ok(HostMetadata {
            instance_id: name,
            zone: zone.split('/').next_back().unwrap_or(&zone).to_string(),
            machine_type: machine_type
                .split('/')
                .next_back()
                .unwrap_or(&machine_type)
                .to_string(),
            extra: serde_json::to_value(info).unwrap_or(serde_json::Value::Null),
        })
    }

    async fn provision_host(&self, _spec: HostSpec) -> Result<HostId, BackendError> {
        Err(BackendError::NotSupported(
            "provision_host on gcp backend (autoscaling lands in Phase 7)",
        ))
    }

    async fn deprovision_host(&self, _id: HostId) -> Result<(), BackendError> {
        Err(BackendError::NotSupported(
            "deprovision_host on gcp backend",
        ))
    }
}
