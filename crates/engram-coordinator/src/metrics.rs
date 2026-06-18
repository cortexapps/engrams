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

    // ADR 0034: the eviction pipeline is a different latency regime —
    // a fat session's snapshot+upload legitimately runs 60-90s+. The
    // default `_seconds` buckets top out at 30s and would flatten
    // every interesting eviction into the +Inf bucket, so this metric
    // gets its own spread. Precedence is by Matcher kind, not
    // insertion order: the exporter sorts overrides by `Matcher`'s
    // derived Ord (Full < Prefix < Suffix) and takes the first match,
    // so this Full() rule beats the Suffix("_seconds") default.
    let eviction_buckets = &[1.0, 5.0, 15.0, 30.0, 60.0, 90.0, 120.0, 180.0, 300.0];

    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(EVICTION_PIPELINE_SECONDS.to_string()),
            eviction_buckets,
        )
        .expect("install eviction histogram buckets")
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

/// ADR 0045 C1: live-teleport leg timings. Labels: leg =
/// capture|restore|total, outcome = success|error|fallback.
pub const MIGRATION_LEG_SECONDS: &str = "engram_migration_leg_seconds";
/// ADR 0045 C1: live-teleport outcomes. Labels: outcome =
/// migrated|unsupported_fallback|aborted_to_source|parachute|fatal.
pub const MIGRATION_TOTAL: &str = "engram_migration_total";

/// Histogram (ADR 0034). Wall-clock of one successful
/// `evict_session_to_state` pipeline run as driven by the eviction
/// scanner — pause + snapshot + upload + record + destroy. Custom
/// buckets to 300s (see `init`): the pre-0034 bug was precisely that
/// these runs exceed request-timeout scales, so the histogram must
/// resolve the 60-180s band. The original failure mode — pipelines
/// silently cancelled mid-run — would reappear here as nominations
/// without matching pipeline completions.
pub const EVICTION_PIPELINE_SECONDS: &str = "engram_eviction_pipeline_seconds";

/// Counter (ADR 0034). Sessions nominated into the Evicting lane.
/// Labels: `source` = `host` (the host's soft/hard-TTL idle driver
/// via POST idle-eviction-candidates) or `backstop` (the coord's
/// PG-derived L3 detector — should be ~0; a sustained nonzero rate
/// means hosts are going blind to running sandboxes).
pub const EVICTION_NOMINATED_TOTAL: &str = "engram_eviction_nominated_total";

/// Counter (Track A). Active sessions the desync watchdog flagged as
/// wedged — the harness event stream desynced from the run state machine.
/// Label: `signature` = `orphan_after_close` (a run-scoped event with no
/// open run, the `bf3dbbcb` shape) or `stuck_open_run` (a `run_started`
/// with zero progress). Should be ~0; a sustained nonzero rate means
/// harnesses are desyncing — alarm-worthy until the streaming rewrite
/// (ADR 0052) removes the inference that causes it.
pub const HARNESS_DESYNC_DETECTED_TOTAL: &str = "engram_harness_desync_detected_total";

/// Counter (Track A). Non-destructive harness re-handshakes the desync
/// watchdog issued to resync a wedged session. A successful one re-emits
/// `Idle` and the session drops out of the flagged set; a session that
/// keeps getting re-handshaked (its `last_event_at` never advances) is
/// escalated to the eviction lane (counted under
/// `engram_eviction_nominated_total{source="desync_watchdog"}`).
pub const HARNESS_REHANDSHAKE_TOTAL: &str = "engram_harness_rehandshake_total";

/// Counter (ADR 0034 Track A). In-place harness reattaches the desync
/// watchdog issued when `rehandshake` returned `NotFound` (the harness vsock
/// is dead but the FC VM is alive): re-issuing the resume `start_agent` drives
/// agentd's reattach/respawn arm (SIGUSR1 a live-but-wedged harness, or respawn
/// an exited one) without a snapshot/destroy/restore. A successful one re-emits
/// `Idle` and the session leaves the flagged set; persistent failure still
/// escalates to the eviction lane.
pub const HARNESS_INPLACE_REATTACH_TOTAL: &str = "engram_harness_inplace_reattach_total";

/// Counter (ADR 0034). Eviction scanner gave up after the retry
/// budget (20 attempts ≈ 3 min) and fell the session back to
/// HostLost. Should be ~0 — alarm-worthy if rising: it means the
/// snapshot pipeline is persistently failing for some session.
pub const EVICTION_BUDGET_EXHAUSTED_TOTAL: &str = "engram_eviction_budget_exhausted_total";

/// Gauge (ADR 0034). Rows in `status='evicting'` observed by the
/// eviction scanner at the top of each tick — its queue depth.
/// Healthy steady-state drains to 0 between ticks; a climbing value
/// means evictions are arriving faster than pipelines complete.
pub const EVICTION_SCANNER_QUEUE: &str = "engram_eviction_scanner_queue";

/// Counter (ADR 0044 K4). Outcome of every `pick_for_session` scheduling
/// decision — the fleet's demand-pressure signal for autoscaling. Labels:
/// `outcome` = `placed` / `no_capacity` / `image_not_ready`. A rising
/// `no_capacity` rate means the fleet is out of room; the autoscaler scales
/// the node pool up. (Scaling only on this is already late — pair it with
/// `engram_fleet_free_mib` to scale *ahead* of hard rejections.)
pub const SESSION_PLACEMENT_TOTAL: &str = "engram_session_placement_total";

/// Gauge (ADR 0044 K4). Aggregate free guest-RAM *reservation* across
/// non-draining hosts, MiB (`Σ total_mib − used_mib`). The headroom signal:
/// scale the node pool up before this approaches the size of one session.
pub const FLEET_FREE_MIB: &str = "engram_fleet_free_mib";

/// Gauge (ADR 0044 K4). Non-draining hosts the scheduler can place on — the
/// schedulable fleet size the autoscaler drives toward demand.
pub const FLEET_SCHEDULABLE_HOSTS: &str = "engram_fleet_schedulable_hosts";

/// Gauge (ADR 0048). Sessions currently `queued` (waiting for capacity).
/// A sustained nonzero value with a flat fleet size means the autoscaler
/// isn't keeping up (or has hit maxHosts).
pub const SESSIONS_QUEUED: &str = "engram_sessions_queued";

/// Gauge (ADR 0048). Σ `mem_budget_mib` over queued sessions — the RAM the
/// queue is waiting for; the operator scales the fleet to cover it.
pub const SESSIONS_QUEUED_MIB: &str = "engram_sessions_queued_mib";

/// Counter (ADR 0048). Queue-scanner per-session outcomes. Labels:
/// `outcome` = `placed` / `requeued` / `failed` / `timeout`.
pub const QUEUE_OUTCOME_TOTAL: &str = "engram_queue_outcome_total";

/// Counter (issue #231). The per-tick `touch_host_heartbeat` persist
/// failed — the host's `last_heartbeat_at` row did NOT advance even
/// though the agent's heartbeat reached this pod. Sustained nonzero is
/// alarm-worthy: an asymmetric PG failure (this pod's pool saturated
/// while a sibling pod's dead-host detector is healthy) staling a live
/// host's row is exactly what orphans a healthy host's sessions, so
/// the handler now also returns 5xx to engage the host's backoff.
/// No `host_id` label — the cardinality convention above forbids
/// per-host labels; the paired `warn!` carries the id for forensics.
pub const HEARTBEAT_PERSIST_FAILURES_TOTAL: &str = "engram_heartbeat_persist_failures_total";
