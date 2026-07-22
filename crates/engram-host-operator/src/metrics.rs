//! Prometheus metrics exporter for the host-operator.
//!
//! Listens on `ENGRAM_OPERATOR_METRICS_ADDR` (default `0.0.0.0:9102`;
//! coordinator=9090, host-agent=9100, host gRPC=9101). Scraped via the
//! chart's operator PodMonitoring (same `metrics.podMonitoring` gate as
//! the host-agent's).
//!
//! Until 2026-07-22 the autoscaler/wave/roll machinery was entirely
//! log-only — scale decisions, teleport-packed shed waves, and node
//! bring-up time were invisible to the prod-ops dashboard. Same naming
//! convention as the other exporters: `engram_<subsystem>_<thing>_<unit>`,
//! low-cardinality labels.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init(addr: SocketAddr) {
    // Node bring-up (GCE instance create → kubelet join → assets staged →
    // host-agent registered with the coordinator) is minutes-class; the
    // exporter's default `_seconds` buckets cap at 30s and would flatten
    // every sample into +Inf (the #850 disease, caught at authoring time
    // here instead of in prod).
    let node_ready_buckets = &[
        15.0, 30.0, 60.0, 90.0, 120.0, 180.0, 240.0, 300.0, 420.0, 600.0, 900.0, 1800.0,
    ];

    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NODE_READY_SECONDS.to_string()),
            node_ready_buckets,
        )
        .expect("install node-ready histogram buckets");

    match builder.install() {
        Ok(()) => {
            tracing::info!(%addr, "host-operator metrics exporter listening");
        }
        Err(e) => {
            tracing::warn!(error = %e, "host-operator metrics exporter init failed; continuing without metrics");
        }
    }

    // Pre-register every counter so each exports as 0 from boot (the #851
    // convention): these are rare-event counters, and a series that only
    // appears on first increment has no Cloud Monitoring descriptor — which
    // blocks alert/dashboard creation and makes "0 because nothing
    // happened" indistinguishable from "0 because nothing is exported".
    for action in [
        "hold",
        "start_wave",
        "continue_wave",
        "abort_and_grow",
        "abort_only",
    ] {
        ::metrics::counter!(AUTOSCALE_STEP_ACTIONS_TOTAL, "action" => action).absolute(0);
    }
    ::metrics::counter!(AUTOSCALE_GROWS_TOTAL).absolute(0);
    ::metrics::counter!(AUTOSCALE_VICTIMS_REMOVED_TOTAL).absolute(0);
    for reason in ["abort", "drain_timeout"] {
        ::metrics::counter!(AUTOSCALE_VICTIMS_RELEASED_TOTAL, "reason" => reason).absolute(0);
    }
    ::metrics::counter!(AUTOSCALE_STUCK_ROLL_REPAIRS_TOTAL).absolute(0);
    ::metrics::counter!(ROLL_NODES_TOTAL).absolute(0);
}

// ─── metric name constants ────────────────────────────────────────

/// Gauge, label `kind` ∈ {desired, schedulable, physical, grow_target}.
/// The autoscale step's decision inputs, re-set every tick:
/// `desired` = the ADR 0048 queue-aware target, `schedulable` = the
/// coordinator's non-draining host count, `physical` = DaemonSet pods,
/// `grow_target` = desired + stuck-roll availability debt (what
/// `set_size` is asserted toward). desired > schedulable = scale-up
/// pressure; desired < schedulable = shed pressure accumulating
/// hysteresis.
pub const AUTOSCALE_HOSTS: &str = "engram_autoscale_hosts";

/// Gauge, 0/1: victim-annotated nodes exist (a shed wave is in flight).
/// A wave blocks image rolls; one pinned at 1 without
/// `victims_removed_total` moving is a wedged wave.
pub const AUTOSCALE_WAVE_IN_FLIGHT: &str = "engram_autoscale_wave_in_flight";

/// Counter, label `action` ∈ {hold, start_wave, continue_wave,
/// abort_and_grow, abort_only}: every `plan_step` decision. The
/// abort_* arms are queue/scale-up pressure pre-empting a wave —
/// sustained aborts mean the fleet is thrashing between shed and grow.
pub const AUTOSCALE_STEP_ACTIONS_TOTAL: &str = "engram_autoscale_step_actions_total";

/// Counter: `set_size` grow calls (scale-up actuations, including
/// stuck-roll availability-debt surges).
pub const AUTOSCALE_GROWS_TOTAL: &str = "engram_autoscale_grows_total";

/// Counter: wave victims fully retired (drained → node removed →
/// coordinator row deleted). The scale-down success meter.
pub const AUTOSCALE_VICTIMS_REMOVED_TOTAL: &str = "engram_autoscale_victims_removed_total";

/// Counter, label `reason` ∈ {abort, drain_timeout}: wave victims
/// released back to the fleet (uncordoned + de-annotated) instead of
/// removed. `abort` = queue/grow pressure reclaimed the capacity
/// (by design); `drain_timeout` = a host would not drain inside the
/// budget — repeated firings for the same fleet mean sessions that
/// won't teleport (check migration_total outcomes coordinator-side).
pub const AUTOSCALE_VICTIMS_RELEASED_TOTAL: &str = "engram_autoscale_victims_released_total";

/// Counter: roll-stuck nodes successfully repaired (drained + removed +
/// deregistered after replacement capacity landed). Each increment is a
/// timed-out image roll that needed the surge-and-retire path.
pub const AUTOSCALE_STUCK_ROLL_REPAIRS_TOTAL: &str = "engram_autoscale_stuck_roll_repairs_total";

/// Counter: nodes sent through the ADR 0044 K3 drain-gated image roll
/// (the deploy path that replaces host-agent pods node-by-node).
pub const ROLL_NODES_TOTAL: &str = "engram_roll_nodes_total";

/// Histogram (buckets 15s–1800s). K8s Node `creationTimestamp` → the
/// node's host-agent first appearing in the coordinator's host list:
/// the full bring-up pipeline (instance create + kubelet join + node
/// assets staged + register). Emitted once per host by the tracker in
/// `autoscale.rs`; hosts already present at operator boot are seeded
/// silently, so an operator restart never emits stale samples.
pub const NODE_READY_SECONDS: &str = "engram_node_ready_seconds";
