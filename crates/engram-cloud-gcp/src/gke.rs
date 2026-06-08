//! ADR 0044 K4: GKE node-pool actuator — the `NodePoolScaler` impl for GCP.
//!
//! **UNVALIDATED ON REAL INFRA.** No nested-virt GKE pool exists to exercise
//! the full loop yet (it arrives with the K5 cutover), and the policy driving
//! it is uncalibrated (no production load). The `setSize` call + the
//! Workload-Identity token fetch are structurally correct and mirror this
//! crate's metadata-server pattern, but treat this as observe-with-caution
//! until a real pool exercises it.
//!
//! Auth: GKE Workload Identity → the GCE metadata server issues an OAuth token
//! for the pod's bound service account. project / cluster / location are read
//! from the same metadata server, so the only per-fleet config is the
//! node-pool name.

use std::time::Duration;

use async_trait::async_trait;
use engram_core::traits::cloud::NodePoolScaler;
use engram_core::BackendError;
use serde::Deserialize;

const METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";
const CONTAINER_API: &str = "https://container.googleapis.com/v1";

/// Resizes a GKE node pool via the Container API, self-identifying its
/// project/cluster/location from the GKE metadata server.
pub struct GkeNodePoolScaler {
    http: reqwest::Client,
    project: String,
    location: String,
    cluster: String,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
}

impl GkeNodePoolScaler {
    /// Detect project/cluster/location from the GKE metadata server (the
    /// operator runs in-cluster). Fails fast off-GKE so the caller can fall
    /// back to the noop scaler.
    pub async fn detect() -> Result<Self, BackendError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        let project = fetch_meta(&http, "project/project-id").await?;
        let location = fetch_meta(&http, "instance/attributes/cluster-location").await?;
        let cluster = fetch_meta(&http, "instance/attributes/cluster-name").await?;
        tracing::info!(%project, %location, %cluster, "gke scaler: detected cluster identity");
        Ok(Self {
            http,
            project,
            location,
            cluster,
        })
    }

    async fn token(&self) -> Result<String, BackendError> {
        let resp = self
            .http
            .get(format!(
                "{METADATA_BASE}/instance/service-accounts/default/token"
            ))
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        if !resp.status().is_success() {
            return Err(BackendError::Protocol(format!(
                "gke token endpoint returned {}",
                resp.status()
            )));
        }
        Ok(resp
            .json::<TokenResp>()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?
            .access_token)
    }
}

async fn fetch_meta(http: &reqwest::Client, path: &str) -> Result<String, BackendError> {
    let resp = http
        .get(format!("{METADATA_BASE}/{path}"))
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|e| BackendError::Sdk(Box::new(e)))?;
    if !resp.status().is_success() {
        return Err(BackendError::Protocol(format!(
            "gke metadata {path} returned {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map(|s| s.trim().to_string())
        .map_err(|e| BackendError::Sdk(Box::new(e)))
}

#[async_trait]
impl NodePoolScaler for GkeNodePoolScaler {
    async fn set_size(&self, node_pool: &str, desired: u32) -> Result<(), BackendError> {
        let token = self.token().await?;
        let url = format!(
            "{CONTAINER_API}/projects/{}/locations/{}/clusters/{}/nodePools/{}:setSize",
            self.project, self.location, self.cluster, node_pool
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "nodeCount": desired }))
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::Protocol(format!(
                "gke setSize returned {status}: {body}"
            )));
        }
        // setSize returns a long-running Operation; we don't poll it — the next
        // reconcile re-asserts the desired size idempotently.
        tracing::info!(node_pool, desired, "gke: setSize accepted");
        Ok(())
    }
}
