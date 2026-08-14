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
const COMPUTE_API: &str = "https://compute.googleapis.com/compute/v1";

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

/// The slice of a GKE `nodePools.get` response we read: the managed
/// instance groups backing the pool. (One per zone for a regional pool;
/// one for a zonal pool.)
#[derive(Deserialize)]
struct NodePoolResp {
    #[serde(default, rename = "instanceGroupUrls")]
    instance_group_urls: Vec<String>,
}

/// Parse a GKE `instanceGroupUrls` entry into `(zone, igm_name)`. The URL
/// is `.../projects/<proj>/zones/<zone>/instanceGroupManagers/<name>`
/// (GKE's per-zone managed instance groups). Returns `None` on a shape we
/// don't recognize, so the caller treats it as "not the owning group".
fn parse_igm_url(url: &str) -> Option<(String, String)> {
    let after_zone = url.split_once("/zones/")?.1; // "<zone>/instanceGroupManagers/<name>"
    let (zone, rest) = after_zone.split_once('/')?;
    let igm = rest.strip_prefix("instanceGroupManagers/")?;
    if zone.is_empty() || igm.is_empty() {
        return None;
    }
    Some((zone.to_string(), igm.to_string()))
}

/// Does the managed instance group `igm_name` own the node `node_name`?
/// GKE names a node `gke-<cluster>-<pool>-<hash>-<suffix>` and its MIG
/// `gke-<cluster>-<pool>-<hash>-grp`; stripping the MIG's `-grp` yields the
/// node-name prefix (the `-` boundary avoids a hash being a prefix of a
/// longer one).
fn igm_owns_node(igm_name: &str, node_name: &str) -> bool {
    match igm_name.strip_suffix("-grp") {
        Some(prefix) => node_name.starts_with(&format!("{prefix}-")),
        None => false,
    }
}

impl GkeNodePoolScaler {
    /// Detect project/cluster/location from the GKE metadata server (the
    /// operator runs in-cluster). Fails fast off-GKE so the caller can fall
    /// back to the noop scaler.
    pub async fn detect() -> Result<Self, BackendError> {
        let http = engram_tls::client_builder()
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

    /// Remove ONE node via Compute `instanceGroupManagers.deleteInstances`,
    /// which both terminates the instance AND decrements the MIG's
    /// targetSize (so no replacement is created — the count-only `setSize`
    /// can't do this without picking an arbitrary victim).
    ///
    /// Steps: `nodePools.get` → the pool's per-zone MIG URLs → match the one
    /// whose name owns this node → `deleteInstances` on that MIG with
    /// `skipInstancesOnValidationError: true` so a node already gone is a
    /// no-op success (idempotent). No LRO polling — same posture as
    /// `set_size`; the next reconcile re-observes the fleet.
    async fn remove_node(&self, node_pool: &str, node_name: &str) -> Result<(), BackendError> {
        let token = self.token().await?;
        // 1. Fetch the pool's managed instance groups.
        let np_url = format!(
            "{CONTAINER_API}/projects/{}/locations/{}/clusters/{}/nodePools/{}",
            self.project, self.location, self.cluster, node_pool
        );
        let resp = self
            .http
            .get(&np_url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::Protocol(format!(
                "gke nodePools.get returned {status}: {body}"
            )));
        }
        let np: NodePoolResp = resp
            .json()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;

        // 2. Find the MIG that owns this node (by hash-prefix match).
        let owner = np
            .instance_group_urls
            .iter()
            .filter_map(|u| parse_igm_url(u))
            .find(|(_, igm)| igm_owns_node(igm, node_name));
        let Some((zone, igm)) = owner else {
            // No MIG claims this node → it's already out of the pool. The
            // wave driver may re-issue after a restart; treat as done.
            tracing::info!(
                node_pool,
                node_name,
                "gke: no instance group owns this node — already removed (idempotent)"
            );
            return Ok(());
        };

        // 3. deleteInstances — removes THIS instance and decrements targetSize.
        let url = format!(
            "{COMPUTE_API}/projects/{}/zones/{zone}/instanceGroupManagers/{igm}/deleteInstances",
            self.project
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "instances": [format!("zones/{zone}/instances/{node_name}")],
                "skipInstancesOnValidationError": true,
            }))
            .send()
            .await
            .map_err(|e| BackendError::Sdk(Box::new(e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::Protocol(format!(
                "gke deleteInstances returned {status}: {body}"
            )));
        }
        tracing::info!(
            node_pool,
            node_name,
            zone,
            igm,
            "gke: deleteInstances accepted"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_igm_url_extracts_zone_and_name() {
        let url = "https://www.googleapis.com/compute/v1/projects/p/zones/us-west2-a/\
                   instanceGroupManagers/gke-mycluster-kvm-abc123-grp";
        assert_eq!(
            parse_igm_url(url),
            Some((
                "us-west2-a".to_string(),
                "gke-mycluster-kvm-abc123-grp".to_string()
            ))
        );
    }

    #[test]
    fn parse_igm_url_rejects_unexpected_shapes() {
        assert_eq!(parse_igm_url("https://example.com/no/zones/here"), None);
        assert_eq!(parse_igm_url("garbage"), None);
        // Has /zones/ but no instanceGroupManagers segment.
        assert_eq!(
            parse_igm_url("https://x/projects/p/zones/z/instances/i"),
            None
        );
    }

    #[test]
    fn igm_owns_node_matches_on_the_hash_prefix() {
        let igm = "gke-mycluster-kvm-abc123-grp";
        // The node carries the MIG's prefix (minus -grp) + its own suffix.
        assert!(igm_owns_node(igm, "gke-mycluster-kvm-abc123-x7p9"));
        // A node from a different MIG (different hash) is NOT owned.
        assert!(!igm_owns_node(igm, "gke-mycluster-kvm-def456-x7p9"));
        // A different pool entirely.
        assert!(!igm_owns_node(igm, "gke-mycluster-bigmem-abc123-x7p9"));
        // A non-MIG name (no -grp suffix) owns nothing.
        assert!(!igm_owns_node(
            "gke-mycluster-kvm-abc123",
            "gke-mycluster-kvm-abc123-x7p9"
        ));
    }

    #[test]
    fn igm_owns_node_requires_the_dash_boundary() {
        // A hash that is a string-prefix of a longer hash must NOT match —
        // the `-` boundary guards against `abc12` matching `abc123-...`.
        let igm = "gke-c-p-abc12-grp";
        assert!(!igm_owns_node(igm, "gke-c-p-abc123-x7p9"));
        assert!(igm_owns_node(igm, "gke-c-p-abc12-x7p9"));
    }
}
