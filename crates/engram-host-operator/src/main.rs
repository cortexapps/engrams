//! ADR 0044 K3: the host-fleet rollout operator.
//!
//! Watches `HostFleet` resources and drives a drain-gated, node-by-node roll
//! of the host-agent DaemonSet toward the CR's target images. The K1 chart
//! uses `updateStrategy: OnDelete` precisely so nothing rolls a host-agent
//! pod out from under a live microVM; this operator is the thing that *does*
//! roll — after draining each node's coordinator host.

mod autoscale;
mod coord;
mod crd;
mod error;
mod metrics;
mod reconcile;
mod scaler;
mod time_source;
mod wave;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::{watcher, Controller};
use kube::{Client, CustomResourceExt};
use tracing_subscriber::EnvFilter;

use crate::crd::HostFleet;
use crate::error::OperatorError;
use crate::reconcile::{reconcile, Ctx};

#[tokio::main]
async fn main() -> Result<(), OperatorError> {
    // `engram-host-operator crd` prints the CRD manifest (JSON — valid YAML,
    // accepted by kubectl/helm) so the chart's crds/ file stays generated from
    // the Rust type (single source of truth).
    if std::env::args().nth(1).as_deref() == Some("crd") {
        let json = serde_json::to_string_pretty(&HostFleet::crd())
            .map_err(|e| OperatorError::Invalid(format!("serialize CRD: {e}")))?;
        println!("{json}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let client = Client::try_default().await?;
    let fleets: Api<HostFleet> = Api::all(client.clone());

    // ADR 0044 K4: select the node-pool scaler. Default noop (logs the desired
    // size); `gke` actuates a GKE pool via the Container API. Off-GKE detection
    // fails fast → fall back to noop so a misconfig never wedges the operator.
    let node_scaler: Arc<dyn engram_core::traits::cloud::NodePoolScaler> =
        match std::env::var("ENGRAM_NODE_POOL_SCALER").as_deref() {
            Ok("gke") => match engram_cloud_gcp::gke::GkeNodePoolScaler::detect().await {
                Ok(s) => {
                    tracing::info!("node-pool scaler: gke");
                    Arc::new(s)
                }
                Err(e) => {
                    tracing::error!(error = %e, "gke scaler init failed (not on GKE?); using noop");
                    Arc::new(scaler::NoopScaler)
                }
            },
            _ => Arc::new(scaler::NoopScaler),
        };
    let metrics_addr = std::env::var("ENGRAM_OPERATOR_METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9102".to_string())
        .parse()
        .map_err(|e| OperatorError::Invalid(format!("ENGRAM_OPERATOR_METRICS_ADDR: {e}")))?;
    metrics::init(metrics_addr);

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        scaler: node_scaler,
        scaledown_hysteresis: crate::autoscale::ScaleDownHysteresis::default(),
        node_ready: crate::autoscale::NodeReadyTracker::default(),
    });

    tracing::info!("engram-host-operator starting; watching HostFleet resources");
    Controller::new(fleets, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => tracing::debug!(fleet = ?obj, "reconciled"),
                Err(e) => tracing::warn!(error = %e, "reconcile error"),
            }
        })
        .await;
    Ok(())
}

/// Requeue on any reconcile error — transient API / coordinator hiccups
/// resolve on the next tick; a drain timeout means the operator should retry
/// the gate rather than abandon the roll.
fn error_policy(_obj: Arc<HostFleet>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(error = %err, "reconcile failed; requeueing");
    Action::requeue(Duration::from_secs(15))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_crd_matches_generated_schema() {
        let checked_in: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/helm/engram-host-fleet/crds/hostfleet.engram.io.yaml"
        ))
        .expect("the checked-in HostFleet CRD must be valid JSON");
        let generated =
            serde_json::to_value(HostFleet::crd()).expect("the generated CRD must serialize");

        assert_eq!(
            checked_in, generated,
            "regenerate the checked-in HostFleet CRD with `cargo run -q -p engram-host-operator -- crd`"
        );
    }
}
