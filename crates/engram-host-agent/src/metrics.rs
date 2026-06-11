//! Prometheus metrics exporter for the host-agent.
//!
//! Listens on `ENGRAM_HOST_METRICS_ADDR` (default `0.0.0.0:9100`),
//! the Prometheus scrape target (a k8s ServiceMonitor scrapes this
//! port).
//!
//! Same naming convention as the coord's metrics module:
//! `engram_<subsystem>_<thing>_<unit>`, low-cardinality labels.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init(addr: SocketAddr) {
    // Sandbox-boot histograms care about sub-second resolution at
    // the fast end — same buckets the coord uses, kept in sync so
    // dashboards can stack the two views.
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
            tracing::info!(%addr, "host-agent metrics exporter listening");
        }
        Err(e) => {
            tracing::warn!(error = %e, "host-agent metrics exporter init failed; continuing without metrics");
        }
    }
}

// ─── metric name constants ────────────────────────────────────────

/// Histogram. Wall-clock time spent in each phase of the sandbox
/// lifecycle on this host. Labels:
/// - `phase`: which sub-step. Emitted today:
///   - `image_resolve` (`pooled_backend::create` — sum of
///     `ensure_image` + `ensure_harness_ext4`, both OCI/cache
///     lookups)
///   - `materialize` (`pooled_backend::create` — `resolve_rootfs`
///     spawns the NBD daemon or materializes the rootfs to file)
///   - `fc_boot` (`FirecrackerBackend::create` — FC API
///     PUT-boot-source / PUT-drive / PUT-vsock / InstanceStart)
///   - `create_total` (`pooled_backend::create` — rollup of the
///     three above; `agent_handshake` is a separate gRPC call)
///   - `agent_handshake` (entry-point emits this; covers
///     `notify_session_policy` + waiting on agentd's ready dial +
///     the SpawnHarness round-trip):
///     ADR 0015 M5: cold-create is the only path; warm pool was
///     retired. Emitted from `grpc_server::start_agent`. Time is
///     dominated by in-VM boot: kernel + engram-init + ext4 mount +
///     agentd bind before its readiness dial reaches the host.
/// - `outcome`: `success` / `invalid_spec` / `fc_error`.
/// - `kind`: `cold` only (kept as a label for future warm-pool v2).
///
/// Pairs with the coord-side `engram_session_boot_seconds` — same
/// operation, different vantage; comparing the two surfaces gRPC
/// round-trip overhead. Drill-down dashboard query example:
/// `histogram_quantile(0.95, sum by (phase, kind, le) (rate(
/// engram_sandbox_boot_seconds_bucket[5m])))`.
pub const SANDBOX_BOOT_SECONDS: &str = "engram_sandbox_boot_seconds";

/// Counter. Sandboxes the host has been asked to create, labelled
/// by `outcome` (`success` / `invalid_spec` / `image_pull_failed`
/// / `fc_error`).
pub const SANDBOX_CREATE_TOTAL: &str = "engram_sandbox_create_total";

/// Gauge. Live sandboxes on this host. Should match
/// `running_sandboxes` in the heartbeat payload.
pub const SANDBOXES_RUNNING: &str = "engram_sandboxes_running";

/// Histogram. Bytes pulled from the OCI registry per session,
/// labelled by `cache` (`hit` / `miss`). Hit-rate gives us a read
/// on whether chunk caching is paying off.
pub const OCI_PULL_BYTES: &str = "engram_oci_pull_bytes";

/// ADR 0014 issue #4: gauge of free disk on the host's work_dir
/// (where snapshot dirs land). Sampled on each idle-evict tick.
/// Drops below `ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES` → idle-evict
/// pauses pushing candidates, the
/// `engram_host_idle_evict_disk_pressure_holds_total` counter
/// increments, and ops can page on the cross-over before disk fills.
pub const HOST_DISK_FREE_BYTES: &str = "engram_host_disk_free_bytes";

/// ADR 0014 issue #4: counter incremented each time the idle-evict
/// tick observes free disk below the floor and skips pushing
/// candidates. Sustained increments mean a snowballing snapshot
/// writer (or some other on-disk leak) is winning the race.
pub const IDLE_EVICT_DISK_PRESSURE_HOLDS_TOTAL: &str =
    "engram_host_idle_evict_disk_pressure_holds_total";

/// ADR 0022 Option A: gauges of summed guest memory across this host's
/// live FC sandboxes, sampled each heartbeat tick from
/// `/proc/<pid>/smaps_rollup`. The **density signal**:
/// `engram_sandbox_guest_pss_bytes / engram_sandbox_guest_rss_bytes` ≈ 1.0
/// when every sandbox holds a private working-set copy (UFFD), and falls
/// below 1.0 as same-template siblings `MAP_PRIVATE`-share one resident
/// base memfile (File backend). PSS charges shared clean pages
/// proportionally to their mapcount, so Σpss ≈ physical RAM actually used
/// and Σrss ≈ the naive no-sharing cost. Productized substrate for the
/// density measurement that gates the ADR 0022 Accepted flip (and the
/// metric a later UI ADR reads). Absent on VZ/non-Linux backends.
pub const SANDBOX_GUEST_PSS_BYTES: &str = "engram_sandbox_guest_pss_bytes";
pub const SANDBOX_GUEST_RSS_BYTES: &str = "engram_sandbox_guest_rss_bytes";

/// ADR 0038 B0: histogram of the FC memory-capture (`PUT /snapshot/
/// create`) wall-clock — the previously-invisible step that hung for
/// 60 s on the cold Full seed (UFFD fault storm). Labels:
/// - `type`: `full` (chain seed / no prior chain) | `diff` (sparse or
///   rolling incremental). After B2 the `full` count should fall toward
///   zero on the resume path; `diff` stays cheap.
/// - `outcome`: `success` | `error`.
///
/// This is the authoritative signal that the 60 s hang is gone.
pub const SNAPSHOT_CREATE_SECONDS: &str = "engram_snapshot_create_seconds";
/// ADR 0045 D5 + issue #147: wall-clock of the snapshot POST phase (disk
/// upload + memory re-chunk + portable blobs) — the previously-invisible
/// half of "the snapshot is just slow". Labels: type=full|diff, outcome.
pub const SNAPSHOT_FINISH_SECONDS: &str = "engram_snapshot_finish_seconds";

/// ADR 0038 B0: histogram of how long a capture waited to acquire the
/// per-sandbox capture lock. The gridlock signal — the 5fadd364
/// incident showed 52–151 s waits as captures queued behind a hung
/// one. With B1 a periodic checkpoint skips rather than waits, so a
/// long tail here is an eviction/drain blocked on an in-flight capture.
pub const SNAPSHOT_CAPTURE_LOCK_WAIT_SECONDS: &str = "engram_snapshot_capture_lock_wait_seconds";

/// ADR 0038 B1: counter of periodic checkpoints skipped because a
/// capture was already in flight for the sandbox. Sustained increments
/// are expected under load (eviction + periodic contend); a flat zero
/// after deploy would mean the skip path isn't exercised.
pub const CHECKPOINT_SKIPPED_TOTAL: &str = "engram_checkpoint_skipped_total";
