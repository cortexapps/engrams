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
        .expect("install node-ready histogram buckets")
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NODE_BOOTSTRAP_PHASE_SECONDS.to_string()),
            node_ready_buckets,
        )
        .expect("install node-bootstrap phase histogram buckets");

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
    ::metrics::counter!(AUTOSCALE_GROWS_TOTAL).absolute(0);
    for kind in ["shed", "repair"] {
        ::metrics::counter!(AUTOSCALE_VICTIMS_REMOVED_TOTAL, "kind" => kind).absolute(0);
    }
    for reason in ["not_in_plan", "deadline", "pressure", "no_row"] {
        ::metrics::counter!(AUTOSCALE_VICTIMS_RELEASED_TOTAL, "reason" => reason).absolute(0);
    }
    for reason in [
        "bound_sessions",
        "capture_jobs",
        "open_teleports_as_source",
        "open_teleports_as_dest",
        "pending_tombstones",
        "resident_sandboxes",
        "enable_work",
        "no_heartbeat_since_request",
        "not_cordoned",
    ] {
        ::metrics::counter!(AUTOSCALE_RETIRE_BLOCKED_TOTAL, "reason" => reason).absolute(0);
    }
    ::metrics::counter!(AUTOSCALE_INVALID_VICTIMS_TOTAL).absolute(0);
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

/// Gauge: durable victim count, with phase retiring or removing.
pub const AUTOSCALE_VICTIMS: &str = "engram_autoscale_victims";
/// Counter: pending retirement observations, by coordinator blocker kind.
pub const AUTOSCALE_RETIRE_BLOCKED_TOTAL: &str = "engram_autoscale_retire_blocked_total";
/// Counter: invalid victim annotations left unchanged.
pub const AUTOSCALE_INVALID_VICTIMS_TOTAL: &str = "engram_autoscale_invalid_victims_total";

/// Counter: `set_size` grow calls (scale-up actuations, including
/// stuck-roll availability-debt surges).
pub const AUTOSCALE_GROWS_TOTAL: &str = "engram_autoscale_grows_total";

/// Counter: completed removals, with kind shed or repair.
pub const AUTOSCALE_VICTIMS_REMOVED_TOTAL: &str = "engram_autoscale_victims_removed_total";

/// Counter: released victims, by not_in_plan, deadline, pressure, or no_row.
pub const AUTOSCALE_VICTIMS_RELEASED_TOTAL: &str = "engram_autoscale_victims_released_total";

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

/// Histogram, label `phase`: bounded cold-node bootstrap phases derived from
/// the scale request, Kubernetes Node conditions, container status, and the
/// coordinator registration edge. `scale_request_to_registered` is the user
/// wait; the other phases explain that total.
pub const NODE_BOOTSTRAP_PHASE_SECONDS: &str = "engram_node_bootstrap_phase_seconds";
