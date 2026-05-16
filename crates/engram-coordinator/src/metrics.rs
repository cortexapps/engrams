//! Prometheus metrics exporter for the coord.
//!
//! Listens on a separate port (`ENGRAM_METRICS_ADDR`, default
//! `0.0.0.0:9090`) so the main API port stays clean and scrapers
//! reach a bearer-free `/metrics` endpoint. On GKE, the
//! engrams-internal deploy ships a `PodMonitoring` CRD pointing
//! at this port; Google Managed Prometheus scrapes it.
//!
//! Recording convention: emit at the **same site** as the
//! `tracing::info_span` for the operation. Logs give per-event
//! forensics; histograms give aggregate p50/p99 across many.
//!
//! Cardinality discipline: label values are bounded enums. Never
//! attach `session_id` / `host_id` / `sandbox_id` — those are
//! per-event identifiers and explode the time-series count.
//! Acceptable labels right now:
//!  - `phase` — boot lifecycle phase name (handful of values)
//!  - `outcome` — `success` / `failure` (constant pair)
//!  - `kind` — broad category like `cold` vs `warm`
//!
//! Naming follows the Prometheus convention: `engram_<subsystem>_<thing>_<unit>`.
//! Histograms use `_seconds` (Prom recommendation) even when we
//! observe in milliseconds — convert at the call site.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Initialise the Prometheus exporter and register the global
/// `metrics` recorder. Idempotent-ish: a second call is a logged
/// no-op (`metrics::set_global_recorder` returns an error on the
/// second set).
///
/// The exporter spawns its own HTTP listener on `addr`. We hand it
/// the address rather than letting it pick so K8s probes and the
/// `PodMonitoring` CRD have a stable target.
pub fn init(addr: SocketAddr) {
    // Histogram buckets sized for sub-second boot latencies that
    // ADR 0007 targets (warm path: ~100ms; cold path: seconds).
    // The bucket boundaries skew toward the fast end because that's
    // where we want resolution; the upper bound catches outliers
    // without distorting the percentile estimates.
    let buckets = &[
        0.005, 0.010, 0.025, 0.050, 0.100, 0.200, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0,
    ];

    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Suffix("_seconds".to_string()),
            buckets,
        )
        .expect("install histogram buckets");

    match builder.install() {
        Ok(()) => {
            tracing::info!(%addr, "metrics exporter listening");
        }
        Err(e) => {
            tracing::warn!(error = %e, "metrics exporter init failed; continuing without metrics");
        }
    }
}

// ─── metric name constants ────────────────────────────────────────
//
// Centralised so every emission site references the same string and
// renames stay local. Documented at the constant rather than the
// emission site because the metric's contract is what scrapers
// depend on, not the producer.

/// Histogram. Time from `POST /sessions` handler entry to the
/// handler returning the SessionCreated response. Labels:
/// - `phase`: emitted today only as `total` (end-to-end including
///   scheduling, host gRPC, in-VM boot, and the harness
///   handshake). Per-sub-phase emissions live on the host-agent
///   under `engram_sandbox_boot_seconds` (matching `phase` taxonomy:
///   `image_resolve` / `materialize` / `fc_boot` / `agent_handshake`
///   / `warm_lease`) because the coord delegates each phase to the
///   host over gRPC; the coord stack frame only has the rollup to
///   time. Comparing coord-`total` against the sum of host-side
///   phases surfaces network and scheduler overhead.
/// - `outcome`: `success` / `bad_request` / `image_not_enabled` /
///   `scheduling_rejected` / `internal`.
/// - `kind`: `cold` (session took the full create path) or `warm`
///   (warm-pool-leased) or `unknown` (errored before the path was
///   chosen). Sourced from `CreateSessionResponse.kind` on success.
///   The headline ADR 0014 win shows up as
///   `engram_session_boot_seconds_sum{phase="total",kind="warm"}`
///   trending toward sub-1s while `kind="cold"` stays at ~20-25s.
pub const SESSION_BOOT_SECONDS: &str = "engram_session_boot_seconds";

/// Counter. Sessions that reached `Active`. Labels: `outcome`
/// (`success` / `scheduling_rejected` / `sandbox_failed` /
/// `harness_failed`).
pub const SESSION_CREATE_TOTAL: &str = "engram_session_create_total";

/// Gauge. Live `Active` session count, scraped from the in-memory
/// registry on each tick. Use as a sanity check against
/// `SESSION_CREATE_TOTAL{outcome="success"}` cumulative minus
/// terminated.
pub const SESSIONS_ACTIVE: &str = "engram_sessions_active";

/// Gauge. Hosts the coord has live heartbeats from (the in-memory
/// `host_registry`). Should equal the count of `ready` rows in
/// Postgres for the slice of time both views are consistent.
pub const HOSTS_READY: &str = "engram_hosts_ready";
