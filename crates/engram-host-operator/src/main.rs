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
mod upgrade;
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

#[derive(Debug, PartialEq, Eq)]
enum ScalerKind {
    Observe,
    Gke,
    Asg,
}

fn parse_scaler_kind(v: Option<&str>) -> Result<ScalerKind, OperatorError> {
    match v {
        Some("noop") => Ok(ScalerKind::Observe),
        Some("gke") => Ok(ScalerKind::Gke),
        Some("asg") => Ok(ScalerKind::Asg),
        _ => Err(OperatorError::Invalid(
            "ENGRAM_NODE_POOL_SCALER must be explicit: noop, gke, or asg".into(),
        )),
    }
}

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

    // ADR 0126: the Helm hooks of an upgrade.
    match std::env::args().nth(1).as_deref() {
        Some("upgrade-drain") => return upgrade_drain().await,
        Some("upgrade-roll") => return upgrade_roll().await,
        _ => {}
    }

    let scaler_kind = parse_scaler_kind(std::env::var("ENGRAM_NODE_POOL_SCALER").ok().as_deref())?;
    tracing::info!(?scaler_kind, "node-pool actuator selection");
    let client = Client::try_default().await?;
    let fleets: Api<HostFleet> = Api::all(client.clone());

    let node_scaler: Option<Arc<dyn engram_core::traits::cloud::NodePoolScaler>> = match scaler_kind
    {
        ScalerKind::Observe => None,
        ScalerKind::Gke => Some(Arc::new(
            engram_cloud_gcp::gke::GkeNodePoolScaler::detect().await?,
        )),
        ScalerKind::Asg => Some(Arc::new(
            engram_cloud_aws::asg::AsgNodePoolScaler::detect().await?,
        )),
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

fn required_env(name: &str) -> Result<String, OperatorError> {
    std::env::var(name).map_err(|_| OperatorError::Invalid(format!("{name} must be set")))
}

fn coordinator_from_env() -> Result<(String, Option<String>), OperatorError> {
    Ok((
        required_env("ENGRAM_COORDINATOR_URL")?,
        std::env::var("ENGRAM_COORDINATOR_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    ))
}

/// The `pre-upgrade` hook: drain every host the target build cannot adopt,
/// and, with `ENGRAM_UPGRADE_MARKER` set, record that it drained.
async fn upgrade_drain() -> Result<(), OperatorError> {
    let (url, token) = coordinator_from_env()?;
    let timeout_secs: u64 = std::env::var("ENGRAM_UPGRADE_TIMEOUT_SECS")
        .unwrap_or_else(|_| "3600".into())
        .parse()
        .map_err(|e| OperatorError::Invalid(format!("ENGRAM_UPGRADE_TIMEOUT_SECS: {e}")))?;
    let fleet = coord::CoordClient::new(url.clone(), token.clone());
    let sessions = coord::SessionOps::new(url, token, Duration::from_secs(timeout_secs));
    let target = engram_protocol::REATTACH_GENERATION;
    match upgrade::drain(
        &fleet,
        &sessions,
        target,
        Duration::from_secs(timeout_secs),
        Duration::from_secs(5),
    )
    .await?
    {
        upgrade::DrainOutcome::NotNeeded => {
            tracing::info!(
                target,
                "every live host runs the target's reattach generation; no drain"
            );
        }
        upgrade::DrainOutcome::Drained { unreachable } => {
            tracing::info!(
                target,
                ?unreachable,
                "drained every host the target cannot adopt; unreachable sessions resume from their last checkpoint",
            );
            if let Ok(marker) = std::env::var("ENGRAM_UPGRADE_MARKER") {
                let namespace = required_env("POD_NAMESPACE")?;
                upgrade::write_marker(Client::try_default().await?, &namespace, &marker).await?;
            }
        }
    }
    Ok(())
}

/// The host-fleet `post-upgrade` hook: after a drain, roll the host-agent
/// pods onto the new build.
async fn upgrade_roll() -> Result<(), OperatorError> {
    let (url, token) = coordinator_from_env()?;
    let fleet = coord::CoordClient::new(url, token);
    let namespace = required_env("POD_NAMESPACE")?;
    let daemon_set = required_env("ENGRAM_HOST_AGENT_DAEMONSET")?;
    let marker = required_env("ENGRAM_UPGRADE_MARKER")?;
    let operator_rolls = std::env::var("ENGRAM_OPERATOR_ENABLED").as_deref() == Ok("true");
    let outcome = upgrade::roll(
        Client::try_default().await?,
        &fleet,
        &namespace,
        &daemon_set,
        &marker,
        operator_rolls,
    )
    .await?;
    tracing::info!(?outcome, operator_rolls, "upgrade roll");
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
    fn parse_scaler_kind_requires_an_explicit_value() {
        for value in [None, Some(""), Some("unknown")] {
            assert!(matches!(
                parse_scaler_kind(value),
                Err(OperatorError::Invalid(_))
            ));
        }
        for (value, expected) in [
            ("noop", ScalerKind::Observe),
            ("gke", ScalerKind::Gke),
            ("asg", ScalerKind::Asg),
        ] {
            assert_eq!(parse_scaler_kind(Some(value)).unwrap(), expected);
        }
    }

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
