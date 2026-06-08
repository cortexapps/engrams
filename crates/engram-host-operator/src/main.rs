//! ADR 0044 K3: the host-fleet rollout operator.
//!
//! Watches `HostFleet` resources and drives a drain-gated, node-by-node roll
//! of the host-agent DaemonSet toward the CR's target images. The K1 chart
//! uses `updateStrategy: OnDelete` precisely so nothing rolls a host-agent
//! pod out from under a live microVM; this operator is the thing that *does*
//! roll — after draining each node's coordinator host.

mod coord;
mod crd;
mod error;
mod reconcile;

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
    let ctx = Arc::new(Ctx {
        client: client.clone(),
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
