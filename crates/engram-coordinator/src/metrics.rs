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
    // ADR 0020 targets (warm path: ~100ms; cold path: seconds).
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

    // ADR 0036 amendment (issue #538): the prestage wait is minutes-class
    // (dev-brain-sized images pull ~33 GB through a 16-permit semaphore) —
    // same regime as eviction, needs its own spread for the same reason.
    let prestage_buckets = &[
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1200.0, 1800.0,
    ];

    // ADR 0088 addendum (enable-leg overhaul): materialize legs span four
    // decades — a demo-image pull is ~3 s, a dev-brain flatten was measured
    // at 74 min pre-overhaul. Same wide-regime problem as prestage; the
    // spread must keep resolving multi-minute flattens after the overhaul
    // lands so regressions stay visible.
    let materialize_stage_buckets = &[
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 2400.0, 4800.0,
    ];

    // ADR 0048 (queue fairness): queue waits are minutes-scale, not
    // seconds-scale — a stuck queue can wait the full 30-minute timeout.
    // Same Full()-beats-Suffix() precedence as the eviction override above.
    let queue_wait_buckets = &[
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0,
    ];

    // Issue #527 Phase 1: prompt→run-start is the same wide-regime problem
    // as eviction — the prod evidence this metric replaces the proxy for
    // shows p50 ≈24.5s, p90 ≈140s, max 1,703s (a resume can be a full cold
    // FC boot). The default `_seconds` buckets top out at 30s, which would
    // collapse essentially the entire observed distribution into +Inf.
    let prompt_to_run_started_buckets = &[
        1.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0,
    ];

    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(EVICTION_PIPELINE_SECONDS.to_string()),
            eviction_buckets,
        )
        .expect("install eviction histogram buckets")
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(ENABLE_PRESTAGE_SECONDS.to_string()),
            prestage_buckets,
        )
        .expect("install prestage histogram buckets")
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                ENABLE_MATERIALIZE_STAGE_SECONDS.to_string(),
            ),
            materialize_stage_buckets,
        )
        .expect("install materialize-stage histogram buckets")
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(QUEUE_WAIT_SECONDS.to_string()),
            queue_wait_buckets,
        )
        .expect("install queue-wait histogram buckets")
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(PROMPT_TO_RUN_STARTED_SECONDS.to_string()),
            prompt_to_run_started_buckets,
        )
        .expect("install prompt-to-run-started histogram buckets")
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
/// - `phase`:
///   - `total` (default/pre-existing): end-to-end including scheduling,
///     host gRPC, in-VM boot, and the harness handshake. Per-sub-phase
///     emissions live on the host-agent under `engram_sandbox_boot_seconds`
///     (matching `phase` taxonomy: `image_resolve` / `materialize` /
///     `fc_boot` / `agent_handshake` / `warm_lease`) because the coord
///     delegates each phase to the host over gRPC; the coord stack frame
///     only has the rollup to time. Comparing coord-`total` against the
///     sum of host-side phases surfaces network and scheduler overhead.
///   - `coord_prepare` (issue #535): `create_session_core` entry through
///     `reserve_and_persist_create`'s commit — the serial coordinator-side
///     prefix ahead of the (now-concurrent, host-side) restore work.
///     Recorded on the Placed path only (a Queued disposition never
///     dispatches a restore).
///   - `coord_finalize` (issue #535): the restore RPC returning through the
///     `created → active` transition — the coordinator-owned tail after
///     the host hands back a live sandbox. Success path only (an error
///     returns before recording it). These two together are the
///     measurable target for the issue's "coordinator serial tail" claim;
///     `total` minus (`coord_prepare` + `coord_finalize`) is the actual
///     host-side restore RPC wall time.
/// - `outcome`: `success` / `bad_request` / `image_not_enabled` /
///   `scheduling_rejected` / `internal`.
/// - `kind`: `restored` (booted via base-snapshot restore — the
///   normal path, ADR 0020) or `queued` (no capacity, enqueued per
///   ADR 0048) or `unknown` (errored before the path was chosen).
///   Sourced from `CreateSessionResponse.kind` on success.
pub const SESSION_BOOT_SECONDS: &str = "engram_session_boot_seconds";

/// Histogram (issue #535 correction pass). Wall time of the
/// `tokio::join!` in `session_boot.rs::restore_base_on_host` that
/// overlaps the restore leg (`restore_base_on_host` on the host, ~0.4-0.7s)
/// against the env/egress leg (`inject_harness_env` + `resolve_inject_entries`,
/// which can round-trip an external mint-mode connector). Neither
/// `coord_prepare` nor `coord_finalize` (see `SESSION_BOOT_SECONDS`'s
/// `phase` doc) covers this window — `coord_finalize` starts only once the
/// join returns — so a slow env/egress leg (a slow external mint call) was
/// invisible to both. No labels: this is always the two-leg join,
/// recorded unconditionally once it resolves (restore success or
/// failure — the failure path still paid for the full join before
/// erroring out, so it's still the relevant wall time to see).
pub const COORD_BOOT_OVERLAP_SECONDS: &str = "engram_coord_boot_overlap_seconds";

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

/// Counter (ADR 0073). Outbox rows handed to the host relay. The gap
/// between this and OUTBOX_ACKED going nonzero-and-growing is the
/// alarmed "delivered but never acked" signal.
pub const OUTBOX_DELIVERED_TOTAL: &str = "engram_outbox_delivered_total";
/// ADR 0079: enqueue->claim latency (seconds). The kernel's hotness
/// proof — a poll-hop executor shows up here as multi-second p99.
pub const SESSION_OP_CLAIM_LATENCY_SECONDS: &str = "engram_session_op_claim_latency_seconds";
/// ADR 0079: fenced writes observed (a successor re-claimed; the old
/// writer stopped silently). Nonzero under pod churn; a sustained rate
/// without churn means something is double-driving.
pub const SESSION_OP_FENCED_WRITES_TOTAL: &str = "engram_session_op_fenced_writes_total";

/// Count a fenced-out (0-row) session write. One helper so EVERY fence —
/// step markers, transitions, event emits, snapshot records, binding
/// clears, park rungs — lands in the same counter; a fence that skips it
/// is invisible during exactly the partition-race incident class the
/// fences exist to stop (re-review of ADR 0079).
pub fn note_fenced_write() {
    ::metrics::counter!(SESSION_OP_FENCED_WRITES_TOTAL).increment(1);
}
/// ADR 0079: stale running ops re-claimed by the sweep (fence-then-resume).
pub const SESSION_OP_RECLAIMS_TOTAL: &str = "engram_session_op_reclaims_total";
/// ADR 0079 (review finding #1): resume ops that hit their retry budget
/// and failed terminally (the livelock terminator). Should be ~0 in a
/// healthy fleet; a sustained rate means resumes are persistently failing.
pub const SESSION_OP_RESUME_BUDGET_EXHAUSTED_TOTAL: &str =
    "engram_session_op_resume_budget_exhausted_total";
/// ADR 0079 (review finding #5): orphaned Pending sessions (placed but
/// no active create_boot op) re-enqueued by the reclaim sweep backstop.
pub const SESSION_OP_PENDING_ORPHANS_RECOVERED_TOTAL: &str =
    "engram_session_op_pending_orphans_recovered_total";

/// Counter (ADR 0073 phase 4, moved from the host detector). A host
/// whose reported free disk is under the floor had its idle
/// nominations held this tick (ADR 0014 issue #4 brake).
pub const IDLE_EVICT_DISK_PRESSURE_HOLDS_TOTAL: &str =
    "engram_idle_evict_disk_pressure_holds_total";
/// Counter (ADR 0073 phase 4, moved from the host detector). Soft-idle
/// candidates kept resident because the host is not under memory
/// pressure (pressure-aware mode only).
pub const IDLE_EVICT_KEPT_RESIDENT_TOTAL: &str = "engram_idle_evict_kept_resident_total";
/// Counter (ADR 0074 rung 1). Nominated evictions cancelled by a
/// returning user before capture began — each one is a full
/// snapshot+destroy+rebuild (p50 12.2s) the user did not pay.
pub const EVICTION_CANCELLED_TOTAL: &str = "engram_eviction_cancelled_total";
/// Counter (ADR 0074 rung 2). Idle sessions PARKED PAUSED (VM paused in
/// place, not evicted) because the host had memory headroom — each is a
/// full snapshot+destroy+rebuild the returning user avoids.
pub const EVICTION_PARKED_PAUSED_TOTAL: &str = "engram_eviction_parked_paused_total";
/// Counter (ADR 0074 rung 2 ascent). Parked-paused sessions un-paused
/// back to Active because the user returned — the fast-path win the
/// rung buys (un-pause in <100ms vs a full snapshot rebuild).
pub const EVICTION_UNPARKED_PAUSED_TOTAL: &str = "engram_eviction_unparked_paused_total";
/// Counter (ADR 0074 rung reaper). Parked sessions DESCENDED to a full
/// eviction — the user never returned within the dwell cap, or the host
/// came under memory pressure and the parked VM's RAM had to be
/// reclaimed. Labelled by `reason`:
/// - `pressure` — the host lost memory headroom (ADR 0074's intended
///   primary, now normally the ONLY, descent trigger).
/// - `hard_ttl` — the absolute ceiling: parked past the idle hard TTL
///   (8h default) with nobody returning.
/// - `dwell` — the retired clock-based descent, re-armed by an operator
///   via `ENGRAM_PARK_DWELL_SECS` (off by default; see the ADR 0074
///   addendum for why a clock must not reclaim RAM).
///
/// A sustained `dwell`/`hard_ttl` rate with zero `pressure` means the
/// fleet is rebuilding guests (~27s each) that it had the RAM to keep.
pub const EVICTION_PARK_DESCEND_TOTAL: &str = "engram_eviction_park_descend_total";
/// Counter (ADR 0073 phase 4). Heartbeats reporting a RUNNING sandbox
/// for an Active session with NO attached harness — the demoted
/// belt-and-braces liveness alarm (was the desync watchdog's job).
/// Alert on a sustained nonzero rate.
pub const HARNESS_ATTACH_DISAGREEMENT_TOTAL: &str = "engram_harness_attach_disagreement_total";
/// Counter (ADR 0073). Rows terminally acked by a confirming event.
pub const OUTBOX_ACKED_TOTAL: &str = "engram_outbox_acked_total";
/// Counter (ADR 0073). Delivery attempts deferred to backoff.
pub const OUTBOX_DEFERRED_TOTAL: &str = "engram_outbox_deferred_total";
/// Counter (ADR 0073). Rows dropped because the session went terminal.
pub const OUTBOX_DROPPED_TERMINAL_TOTAL: &str = "engram_outbox_dropped_terminal_total";

/// ADR 0045 C1: live-teleport leg timings. Labels: leg =
/// capture|restore|total, outcome = success|error|fallback.
pub const MIGRATION_LEG_SECONDS: &str = "engram_migration_leg_seconds";
/// ADR 0045 C1: live-teleport outcomes. Labels: outcome =
/// migrated|unsupported_fallback|aborted_to_source|parachute|fatal.
pub const MIGRATION_TOTAL: &str = "engram_migration_total";

/// Histogram (ADR 0034). Wall-clock of one successful
/// `run_evict_pipeline` run as driven by the evict verb / eviction
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

// ADR 0079 (review finding #11): the D5 inline finalize row-watch is
// gone — the evict op finishes the instant the session is Idle and the
// host-owned finalize lands the row via the heartbeat reconcile. Its
// `EVICTION_FINALIZE_ROW_WAIT_TIMEOUT_TOTAL` counter went with it.

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
/// `outcome` = `placed` / `failed` / `timeout` / `image_gone`. (ADR 0079:
/// `requeued` is gone — a placed create's boot retries on its create_boot
/// op row instead of bouncing back to the queue.)
pub const QUEUE_OUTCOME_TOTAL: &str = "engram_queue_outcome_total";

/// Histogram (ADR 0048, queue-fairness). `queued_at` → placement/terminal,
/// seconds. This is OUTSIDE `engram_session_boot_seconds`: the create
/// handler returns 201 `{kind:"queued"}` immediately on enqueue, so the
/// entire queue wait previously fell outside every latency histogram we
/// have. Labels:
/// - `origin`: `create` / `resume`.
/// - `outcome`: `placed` (create: the durable `queued → pending` flip;
///   resume: dequeue to `Idle`) / `timeout`.
///
/// Measures wait since the original `queued_at`; once placed, boot retry
/// moves onto the create_boot op row (ADR 0079) and no further samples
/// are emitted for the session.
pub const QUEUE_WAIT_SECONDS: &str = "engram_queue_wait_seconds";

/// Gauge (ADR 0048, queue-fairness). Age in seconds of the oldest queued
/// row (0 when the queue is empty), sampled once per scanner sweep. The
/// "is the queue stuck" pager signal complementing `engram_sessions_queued`
/// (which only tells you the queue is nonempty, not for how long).
pub const QUEUE_HEAD_AGE_SECONDS: &str = "engram_queue_head_age_seconds";

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

/// Histogram (ADR 0036 amendment, issue #538). Wall time of the enable
/// scanner's `prestaging` stage — the fleet chunk-prestage wait between
/// base-snapshot capture and the `enabled_images` upsert. Labels:
/// `outcome` = `complete` (every eligible host staged) / `partial`
/// (deadline hit with ≥1 staged) / `empty_fleet` (vacuous pass, no
/// eligible hosts) / `timeout_zero` (deadline hit with 0 staged —
/// transient, retried under the attempts budget). Minutes-class for a
/// large image; the per-image SLO canary is the end-to-end guard that
/// first-create-after-refresh stays inside budget.
pub const ENABLE_PRESTAGE_SECONDS: &str = "engram_enable_prestage_seconds";

/// Histogram (ADR 0088 addendum: enable-leg overhaul). Wall time of each
/// closed materialize stage, recorded by the enable scanner's progress
/// consumer as the stage timeline advances (and at the success-path close
/// of the final `chunk` stage). Labels: `stage` = `pull` / `flatten` /
/// `pack` / `chunk`. The reset-on-`pull` retry path records nothing — a
/// killed attempt's open stage has no honest duration. This is the
/// before/after instrument for the overhaul's flatten + chunk-pipeline
/// work; the durable twin is `enable_jobs.materialize_stages`.
pub const ENABLE_MATERIALIZE_STAGE_SECONDS: &str = "engram_enable_materialize_stage_seconds";

/// Counter (ADR 0036 amendment, issue #538). Per-host prestage outcomes
/// recorded at the end of each `prestaging` stage. Labels: `outcome` =
/// `staged` / `timed_out` / `unschedulable`. No `host_id` label — the
/// cardinality convention above forbids per-host labels; the durable
/// per-host record lives on the `enable_jobs.prestage_hosts` column.
pub const ENABLE_PRESTAGE_HOST_OUTCOMES_TOTAL: &str = "engram_enable_prestage_host_outcomes_total";

/// Counter (ADR 0068; `origin` label added in the core-ops-batch
/// correction pass; reserve-path reasons added after the 2026-07-11
/// campaign). Per-host exclusion reasons whenever a placement attempt
/// rejects hosts — kills the "no capacity with free hosts" mystery mode.
/// Two emitters:
/// - `placement::log_empty_candidates` — every empty-candidate-set path.
/// - `placement::log_reserve_no_fit` — candidates existed but the
///   FOR-UPDATE 2D pick fit none (previously the LAST silent branch).
///
/// Labels:
/// - `reason` = the bounded `placement::exclusion_summary` vocabulary
///   (`excluded` / `not_ready` / `cordoned` / `wire_skew` / `stale` /
///   `cap:<name>` — one of the ~6 named capabilities, so still bounded
///   / `digest_not_ready` / `no_fit`) PLUS the reserve-path fit
///   vocabulary from `PlacementNoFit` (`ram_full` / `cpu_full` /
///   `unmeasured` / `not_lockable` / `fits_now`).
/// - `origin` = which call site: `create` (fresh session,
///   `api/sessions.rs::boot_prepared`) / `queue_create` (queue-scanner
///   re-placing a create-origin queued session) /
///   `queue_resume_precheck` (queue-scanner's resume dequeue capacity
///   check) / `resume` (the resume/evac path, `pick_for_session`).
///   Bounded to these 4 values — do not add a 5th without updating this
///   doc.
///
/// No `host_id` label — see the cardinality convention above; the
/// paired `warn!` names hosts.
pub const PLACEMENT_EXCLUDED_TOTAL: &str = "engram_placement_excluded_total";

/// Counter (ADR 0068). `reconcile::flip_missing` probed the sandbox
/// directly and found it alive (`process_alive == true`) despite being
/// absent from the host's self-reported `running_sandboxes` — the flip
/// to `host_lost` was skipped and the strike counter reset. Sustained
/// nonzero means the delivery/binding desync class (epic-binding-epoch-
/// delivery) is still producing false `running_sandboxes` misses; this
/// metric graphs how often the probe is rescuing sessions from it.
pub const RECONCILE_PROBE_RESCUES_TOTAL: &str = "engram_reconcile_probe_rescues_total";

/// Counter (ADR 0068). The dead-host detector's own `Ping` probe
/// (`dead_host.rs`, added in `7fcc4c3c`) found the host alive despite a
/// stale `last_heartbeat_at` row, and skipped the eviction. No
/// behavioral change from this issue — added alongside the reconcile
/// rescue counter above so both rescue paths are graphable together.
pub const DEAD_HOST_PROBE_RESCUES_TOTAL: &str = "engram_dead_host_probe_rescues_total";

/// Counter (issue #762). `dead_host::host_lost_straggler_sweep`
/// completed the delayed HostLost stage-2 transition for a row whose
/// inline transition never ran or failed.
pub const HOST_LOST_STRAGGLERS_SETTLED_TOTAL: &str = "engram_host_lost_stragglers_settled_total";

/// Counter (issue #777, ADR 0098 Phase 3 honest-Dead). A HostLost
/// stage-2 transition routed a session to `Dead` while a snapshot row
/// DID exist — the snapshot was un-recoverable (its BlobStorage HEAD
/// failed at take-time) and there was no live disk manifest either.
/// Distinct from "no snapshot at all" so a bad-capture pipeline stays
/// visible instead of hiding behind a generic Dead. Fired from every
/// stage-2 site (`reconcile::flip_missing`, `dead_host::evict_host`,
/// `dead_host::host_lost_straggler_sweep`).
pub const HOST_LOST_UNRECOVERABLE_SNAPSHOT_TOTAL: &str =
    "engram_host_lost_unrecoverable_snapshot_total";

/// Counter (issue #777, ADR 0098 Phase 3 ask-the-host).
/// `host_lost_straggler_sweep` was about to destroy a still-bound
/// sandbox, probed the host, and found the VMM process ALIVE — so it
/// DEFERRED the destroy+settle for the reattach machinery and banked a
/// serving-strike instead. Sustained nonzero means live VMs are sitting
/// under HostLost rows (a partition/desync parking bug upstream); the
/// sweep no longer kills them on sight (removes the >60s-partition
/// destroy-a-live-VM window of #762/#769).
pub const HOST_LOST_STRAGGLER_DEFERRED_SERVING_TOTAL: &str =
    "engram_host_lost_straggler_deferred_serving_total";

/// Counter (ADR 0019 / telemetry restoration #526). Same-host vs
/// cross-host resume split, emitted in `api/snapshot.rs::resume_from_fc_snapshot`
/// once placement resolves. Labels: `placement` = `same_host` (the
/// chosen host == the snapshot record's capturing host — the
/// zero-cost hot-tier hit ADR 0007's local-dir cache exists for) /
/// `cross_host` (relocated — the host materializes from chunked
/// manifests) / `unknown_prior_host` (the record carries no `host_id`,
/// e.g. a pre-ADR-0007 row or one written before the capturing host
/// was recorded — can't classify).
pub const SESSION_RESUME_PLACEMENT_TOTAL: &str = "engram_session_resume_total";

/// Histogram (issue #527 Phase 1). Wall-clock from a `prompt_received`
/// receipt (the first PG write of `send_prompt_core`, before auto-resume)
/// to the matching `run_started{prompt_id}` landing in `session_events`.
/// The true prompt→first-token *lower bound* — replaces the old
/// `idle→created` proxy, which post-dates the resume and therefore
/// undercounts. No labels: this is a single fleet-wide SLO signal, not
/// per-image (the per-image breakdown is the Phase 2 canary's job).
/// Recorded once per run-start that carries a `prompt_id`; the env-seeded
/// initial prompt (no `prompt_id`, no receipt row) never contributes a
/// sample.
pub const PROMPT_TO_RUN_STARTED_SECONDS: &str = "engram_prompt_to_run_started_seconds";
