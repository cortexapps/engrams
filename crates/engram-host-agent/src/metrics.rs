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
    // Capture-pipeline histograms measure GiB-scale work (full memory
    // re-chunk, disk upload, FC snapshot writes) that legitimately runs
    // minutes — the 2026-07-13 incident's full re-chunk ran 40+. With
    // the default 30 s cap every such sample lands in +Inf and the
    // histogram can't distinguish "90 s" from "2 h". Wide log-scale
    // buckets for exactly those metrics; `Matcher::Full` wins over the
    // `_seconds` suffix rule, so this is purely additive.
    let capture_buckets = &[
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 2400.0, 3600.0, 7200.0,
    ];

    let mut builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Suffix("_seconds".to_string()),
            buckets,
        )
        .expect("install histogram buckets");
    for name in [
        RECHUNK_SECONDS,
        SNAPSHOT_CREATE_SECONDS,
        SNAPSHOT_FINISH_SECONDS,
        EVICTION_FINALIZE_STAGE_SECONDS,
    ] {
        builder = builder
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(name.to_string()),
                capture_buckets,
            )
            .expect("install capture histogram buckets");
    }

    // ADR 0101 B: epoch age lives in the pacing clamp band
    // [ENGRAM_CHECKPOINT_MIN_INTERVAL_SECS 30, ENGRAM_CHECKPOINT_INTERVAL_SECS
    // 600], stretching above the backstop by capture time — under the
    // default sub-30 s `_seconds` buckets every sample landed in +Inf.
    let epoch_seconds_buckets = &[
        30.0, 60.0, 90.0, 120.0, 180.0, 240.0, 300.0, 420.0, 600.0, 750.0, 900.0, 1200.0, 1800.0,
    ];
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(CHECKPOINT_EPOCH_SECONDS.to_string()),
            epoch_seconds_buckets,
        )
        .expect("install epoch-seconds histogram buckets");
    // Epoch bytes gets fine resolution around the 256 MiB
    // ENGRAM_CHECKPOINT_TARGET_EPOCH_MB so "is the controller centering
    // on target" is answerable fleet-wide — an unbucketed histogram
    // renders as a Prometheus summary, whose per-host quantiles can't
    // be aggregated. The 0 bucket splits out the no-dirt epochs idle
    // sessions coast through on the backstop.
    const MIB: f64 = 1024.0 * 1024.0;
    let epoch_bytes_buckets = &[
        0.0,
        16.0 * MIB,
        64.0 * MIB,
        128.0 * MIB,
        192.0 * MIB,
        256.0 * MIB,
        320.0 * MIB,
        384.0 * MIB,
        512.0 * MIB,
        1024.0 * MIB,
        4096.0 * MIB,
        16384.0 * MIB,
    ];
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(CHECKPOINT_EPOCH_BYTES.to_string()),
            epoch_bytes_buckets,
        )
        .expect("install epoch-bytes histogram buckets");
    // ADR 0110: the write ack is the one latency the dirty file newly puts
    // on the guest's critical path, and it lives three decimal places
    // below everything else this exporter measures. A `pwrite` into page
    // cache returns in tens of microseconds; the default `_seconds`
    // buckets start at 5 ms, so every healthy sample would land in the
    // first bucket and p99 would report "fast" no matter what happened.
    // The top of the range covers the two known slow paths: a 16 MiB
    // chunk materializing on first touch, and writeback throttling under
    // memory pressure (ADR 0110 tradeoff 1).
    let write_ack_buckets = &[
        0.000_01, 0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05,
        0.1, 0.25, 0.5, 1.0, 5.0,
    ];
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NBD_WRITE_ACK_SECONDS.to_string()),
            write_ack_buckets,
        )
        .expect("install write-ack histogram buckets");
    // One NBD request spans 4 KiB (NBD_BLOCK_SIZE) to 32 MiB
    // (MAX_REQUEST_PAYLOAD_BYTES). Without explicit buckets a histogram
    // renders as a Prometheus summary, whose per-host quantiles cannot be
    // aggregated across the fleet — the same trap the epoch-bytes comment
    // above records.
    const KIB: f64 = 1024.0;
    let write_bytes_buckets = &[
        4.0 * KIB,
        16.0 * KIB,
        64.0 * KIB,
        128.0 * KIB,
        512.0 * KIB,
        MIB,
        4.0 * MIB,
        16.0 * MIB,
        32.0 * MIB,
    ];
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NBD_WRITE_BYTES.to_string()),
            write_bytes_buckets,
        )
        .expect("install write-bytes histogram buckets");
    // The read side reuses the write-ack bucket boundaries so the two
    // distributions compare bucket-for-bucket. The measured landmarks all
    // have a boundary near them: the ~90 µs serve floor (vCPU kept busy)
    // sits in (50 µs, 100 µs], the ~195 µs typical qd=1 round trip in
    // (100 µs, 250 µs], and the 8–9 ms first-touch whole-chunk
    // materialization in (5 ms, 10 ms].
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NBD_READ_SECONDS.to_string()),
            write_ack_buckets,
        )
        .expect("install read histogram buckets");
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(NBD_READ_BYTES.to_string()),
            write_bytes_buckets,
        )
        .expect("install read-bytes histogram buckets");
    // Recovered bytes span one 16 MiB chunk to a whole unpublished
    // divergence. The 0 bucket splits out clean-shutdown recoveries that
    // had nothing left to save from the ones that rescued real writes.
    let recovered_bytes_buckets = &[
        0.0,
        16.0 * MIB,
        64.0 * MIB,
        256.0 * MIB,
        1024.0 * MIB,
        4096.0 * MIB,
        16384.0 * MIB,
    ];
    builder = builder
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(DIRTY_RECOVERED_BYTES.to_string()),
            recovered_bytes_buckets,
        )
        .expect("install recovered-bytes histogram buckets");

    match builder.install() {
        Ok(()) => {
            tracing::info!(%addr, "host-agent metrics exporter listening");
        }
        Err(e) => {
            tracing::warn!(error = %e, "host-agent metrics exporter init failed; continuing without metrics");
        }
    }

    // Pre-register alert-critical zero-normally counters so they export
    // as 0 from boot. A counter that only increments on disaster has no
    // metric descriptor until the disaster happens, and Cloud Monitoring
    // refuses to create an alert policy on a metric it has never seen —
    // the alert must exist before the first firing, so the series must
    // too. (Same rationale as the coordinator's pre-registration.)
    ::metrics::counter!(CHECKPOINT_CHAIN_POISONED_TOTAL).absolute(0);
    ::metrics::counter!(SPOOL_LINEAGE_MISMATCH_TOTAL).absolute(0);
    // ADR 0110: a failed extent scan fails the reattach. Pre-register so
    // the alert policy can exist before the first firing.
    ::metrics::counter!(DIRTY_RECOVER_TOTAL, "outcome" => "scan_failed").absolute(0);
    ::metrics::counter!(SHUTDOWN_STAGE_PANIC_TOTAL).absolute(0);
    ::metrics::counter!(CAPTURE_SHUTDOWN_STRAGGLER_TOTAL).absolute(0);
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

/// Histogram of the ABSOLUTE wall-clock offset (seconds) the host had to
/// push into a guest at `start_agent`, emitted by
/// `FirecrackerBackend::step_guest_clock`.
///
/// FC freezes `CLOCK_REALTIME` at snapshot capture, so a restored guest
/// starts however old its base snapshot is. agentd normally re-steps
/// itself from the KVM PTP device; this metric records what the HOST had
/// to correct, which is the part the host can actually verify.
///
/// - `outcome`: `stepped` (round trip succeeded; the recorded value is
///   the applied offset, `0` when the guest was already inside the 2 s
///   threshold), `unsupported` (agentd's wire predates `StepClock`),
///   `failed` (no round trip inside the budget). The last two record
///   `0` — they carry no offset, only their own count.
///
/// **What to alarm on.** A `stepped` p99 that climbs with an image's
/// snapshot age means that image's guests have lost PTP self-sync and
/// are running purely on this push; a sustained non-zero `unsupported`
/// or `failed` rate means guests are running on an UNCORRECTED clock,
/// which surfaces far away and much later as TLS failures against any
/// upstream whose certificate was issued after the snapshot
/// (`SSL certificate is not yet valid` — the 2026-08-07 review outage).
pub const GUEST_CLOCK_STEP_SECONDS: &str = "engram_guest_clock_step_seconds";

/// ADR 0070: gauge of `fs_free - fs_total * ENGRAM_KUBELET_EVICT_PCT/100`
/// on the host's `work_dir` mount — how far free disk sits above the
/// kubelet's ephemeral-storage hard-eviction line. Sampled every
/// heartbeat tick (`UtilizationProbe::sample`), independent of the
/// chunk-cache budget: this alarms on TOTAL disk pressure (snapshots,
/// memfiles, the OCI cache, anything sharing the mount), not just the
/// cache's own slice. Negative or shrinking toward zero means the
/// kubelet is about to evict this host-agent pod — page BEFORE that
/// happens, since the eviction itself is the amplifier (pod churn
/// orphans the local chunk cache, forcing every subsequent resume onto
/// the ADR-0028 disk-only cold-recovery path).
pub const HOST_DISK_HEADROOM_TO_KUBELET_BYTES: &str = "engram_host_disk_headroom_to_kubelet_bytes";

/// ADR 0070: gauge of summed on-disk bytes of every per-template base
/// memfile the image-prefetch supervisor has materialized (ADR 0022
/// Option A residency) — these are unevictable disk (mlock'd, reclaimed
/// only on image-disable), so they're part of the same "floor the
/// budget can't touch" accounting as pinned chunk bytes. Sampled each
/// image-prefetch reconcile tick. `generation-purge` (a later item in
/// the 2026-07 overhaul) deletes File-mode memfiles entirely, taking
/// this term to zero — this gauge exists to measure that, not to grow
/// more machinery around it.
pub const HOST_BASE_MEMFILE_BYTES: &str = "engram_host_base_memfile_bytes";

/// Tier 1 (pressure-aware idle eviction): gauge of free physical RAM as a
/// percent of `MemTotal`, sampled each idle-evict tick when
/// `ENGRAM_IDLE_EVICT_PRESSURE_AWARE` is on. Below
/// `ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT`, soft-idle sandboxes become eligible
/// for reclamation; above it they stay resident. Alarm on a sustained
/// approach to the floor.
pub const HOST_MEM_FREE_PCT: &str = "engram_host_mem_free_pct";

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
/// Issue #540: the same Σpss/Σrss density signal, but for sandboxes
/// flagged `parked` (RAM-resident, reservation-free — epic-parking-
/// ladder rungs 2-3). Always 0 until a backend ever parks a sandbox.
/// Gauge-only: never folded into `allocatable_mib`. Without these, the
/// sharing-credit rule (`Σpss/Σrss < 1.0`, ADR 0046) can't be evaluated
/// for parked residents — exactly the population density math cares
/// about once the ladder lands.
pub const SANDBOX_GUEST_PARKED_PSS_BYTES: &str = "engram_sandbox_guest_parked_pss_bytes";
pub const SANDBOX_GUEST_PARKED_RSS_BYTES: &str = "engram_sandbox_guest_parked_rss_bytes";

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
/// Incident 2026-07-13: phase breakdown of a FULL memory re-chunk
/// (`chunk_memory_to_store`) — the multi-GiB scan+upload that a capture
/// pays when it has no checkpoint chain. `snapshot_finish_seconds`
/// wraps it whole; this splits where the time went. Labels:
/// - `phase`: `scan` (sequential read + zero-check of the memory image)
///   | `upload` (window flushes: hash + dedup HEAD + GCS PUT).
/// - `source`: `snapshot_finish` (composed snapshot post phase) |
///   `evict_finalize` (eviction finalize job).
/// - `outcome`: `success` | `error`.
pub const RECHUNK_SECONDS: &str = "engram_rechunk_seconds";
/// Counter siblings of [`RECHUNK_SECONDS`] (label: `source`): total
/// bytes scanned from memory images vs bytes actually uploaded (the
/// non-zero remainder). The ratio is the zero-elision density — low
/// density means the scan phase dominates by construction.
pub const RECHUNK_BYTES_SCANNED_TOTAL: &str = "engram_rechunk_bytes_scanned_total";
pub const RECHUNK_BYTES_UPLOADED_TOTAL: &str = "engram_rechunk_bytes_uploaded_total";

/// ADR 0088 addendum: histogram of each closed capture-timeline leg's
/// wall-clock, recorded by the capture-job executor as its synthetic
/// `[capture]` legs close. Labels: `leg` = `boot` |
/// `cold_base_memory_dump` | `warm_hook` | `cold_base_upload` |
/// `final_snapshot` (slug of the leg name). The capture pipeline in
/// one query — `SNAPSHOT_CREATE/FINISH_SECONDS` remain the per-
/// snapshot-call views; this is the per-enable-leg view the overhaul
/// is benchmarked against. Warm-hook SUB-stages ride the existing
/// `engram_warm_stage_seconds` (see `record_warm_stage_metrics`).
pub const CAPTURE_LEG_SECONDS: &str = "engram_capture_leg_seconds";

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

/// ADR 0112 D3: counter of pre-capture swap disarms refused, labeled
/// by capture flavor (`terminal` / `periodic`). Periodic refusals are
/// the designed degradation under memory pressure (the continuously
/// flushed disk is that tick's checkpoint); a SUSTAINED rate is the
/// operator signal "this image is under-sized — raise
/// `suggested_memory_mib`". Terminal refusals mean an eviction could
/// not capture memory and requeued — rare, alarm-worthy.
pub const SWAP_DISARM_REFUSED_TOTAL: &str = "engram_swap_disarm_refused_total";

/// ADR 0112 D3: histogram of successful pre-capture swap disarm
/// duration (meminfo probe + `swapoff -a` page-back-in). Steady state
/// is one exec round trip (≈0 used swap); the tail scales with used
/// swap, bounded by the RAM/4 device size.
pub const SWAP_DISARM_SECONDS: &str = "engram_swap_disarm_seconds";

/// ADR 0112 D5: Σ `swap_mib` over live sandboxes, in bytes — the worst
/// case ephemeral swap can allocate on the work_dir mount (the backing
/// inodes are anonymous, so committed is the only honest attribution).
/// Also fed into the chunk cache's co-tenant reserve and the heartbeat.
pub const HOST_COMMITTED_SWAP_BYTES: &str = "engram_host_committed_swap_bytes";

/// ADR 0112 D5 (closing ADR 0110's open accounting gap): allocated
/// bytes under the dirty root — the per-sandbox dirty files' real
/// footprint on the shared mount, fed into the co-tenant reserve.
pub const HOST_DIRTY_FILES_BYTES: &str = "engram_host_dirty_files_bytes";

/// ADR 0101 B: dirty bytes one diff epoch carried (the adaptive
/// controller aims this at `ENGRAM_CHECKPOINT_TARGET_EPOCH_MB`) and the
/// epoch's wall-clock length. Together they surface the controller's
/// behavior: bytes far above target = the controller is floor-clamped
/// (raise the target or lower the floor); epochs pinned at the max =
/// idle sessions coasting on the backstop, as designed. Both are
/// histograms with explicit buckets installed in [`init`] — seconds
/// spans the pacing clamp band, bytes centers on the 256 MiB target
/// (the 2026-07-21 deploy shipped without them: seconds was all-+Inf
/// under the default 30 s ceiling, bytes rendered as an
/// unaggregatable per-host summary).
pub const CHECKPOINT_EPOCH_BYTES: &str = "engram_checkpoint_epoch_bytes";
pub const CHECKPOINT_EPOCH_SECONDS: &str = "engram_checkpoint_epoch_seconds";

/// Incident 2026-07-10: a Diff capture failed AFTER Firecracker consumed
/// (and reset) the KVM dirty-page bitmap, so its dirty set is
/// unrecoverable and the sandbox's checkpoint chain was dropped — the
/// next capture takes a FULL snapshot instead of a silently-incomplete
/// Diff. Each increment is a corruption event AVOIDED; sustained
/// increments mean the re-chunk/persist leg is unhealthy (find out why —
/// Fulls are expensive) but never mean data loss.
pub const CHECKPOINT_CHAIN_POISONED_TOTAL: &str = "engram_checkpoint_chain_poisoned_total";

/// 2026-08-03 `chain_poisoned` alert: a capture was still in flight when
/// the SIGTERM ladder's capture-drain deadline fired. The process exit
/// that follows cancels the capture's post-processing and poisons its
/// chain (a `CHECKPOINT_CHAIN_POISONED_TOTAL` increment with
/// `failed_step="snapshot post-processing"`). Attributes shutdown-overrun
/// poisons; a sustained rate means the drain budget is too small for the
/// fleet's capture sizes.
pub const CAPTURE_SHUTDOWN_STRAGGLER_TOTAL: &str = "engram_capture_shutdown_straggler_total";

/// Issue #529: an `EvictionFinalizeRecord` (+ its `disk-pending/` chunk
/// files, when the capture had a dirty disk tier) was durably persisted
/// before `snapshot_begin` returned — the durability boundary moved
/// from "upload complete" to "this instant". Should track 1:1 with
/// D5 eviction attempts on FC hosts.
pub const EVICTION_FINALIZE_PERSISTED_TOTAL: &str = "engram_eviction_finalize_persisted_total";
/// Issue #529: an eviction finalize job reached its terminal stage —
/// the durable `CheckpointRecord { kind: EvictionFinal }` was written
/// (to be reconciled into PG on the next heartbeat) and the finalize
/// record was deleted.
pub const EVICTION_FINALIZE_COMPLETED_TOTAL: &str = "engram_eviction_finalize_completed_total";
/// Issue #529: `resume_pending_finalizes` re-drove a persisted record
/// at host-agent startup — the crash-recovery path actually firing.
pub const EVICTION_FINALIZE_REDRIVEN_TOTAL: &str = "engram_eviction_finalize_redriven_total";
/// Issue #529: a finalize job exhausted `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS`
/// and was quarantined (`finalize/failed/`) — never silent; resume falls
/// back to the prior periodic checkpoint. Should stay at/near zero.
pub const EVICTION_FINALIZE_QUARANTINED_TOTAL: &str = "engram_eviction_finalize_quarantined_total";
/// Issue #529: wall-clock per finalize leg. Label `stage` =
/// `disk`|`memory`|`blobs`|`terminal`.
pub const EVICTION_FINALIZE_STAGE_SECONDS: &str = "engram_eviction_finalize_stage_seconds";
/// ADR 0019 / telemetry restoration (#526): the resume-prefault
/// effectiveness detector. The uffd-handler writes a per-jail
/// `prefault-stats.json` (sibling of `working-set-trace.json`) at the
/// end of `prefault_from_trace` (and, for the no-trace case, from
/// `main.rs` before the fault loop starts); `restore()` here reads it
/// after a resume completes and increments this by `outcome`:
/// - `replayed`: a trace was loaded and prefault ran (installed may
///   still be 0 — see `PREFAULT_CHUNKS_TOTAL`).
/// - `no_trace`: no working-set trace was requested/loaded for this
///   resume (base image, migration dest, or publish never landed).
/// - `stats_missing`: a trace WAS requested but the stats file isn't
///   there at all — the alarm condition. This is the exact class of
///   bug that went inert 3x silently (d0e5ecf3, cf6e4d32, 7c2a7226):
///   the handler died, or the wiring silently didn't fire, and nothing
///   said so until someone went looking weeks later.
///
/// Convergence note: `prefault-admission-control` (same overhaul) reuses
/// this exact counter name/labels against its own superset gate-file
/// schema — this crate must not introduce a parallel `engram_prefault_*`
/// family alongside it.
pub const RESUME_PREFAULT_TOTAL: &str = "engram_resume_prefault_total";

/// Companion to [`RESUME_PREFAULT_TOTAL`]: per-chunk install outcome
/// from the same `prefault-stats.json`, labeled `result` =
/// `installed` | `skipped`. A `replayed` outcome with `installed == 0`
/// is itself worth alarming on (trace loaded, but the session-manifest
/// no longer needs any chunk it names — a stale/mismatched trace).
pub const RESUME_PREFAULT_CHUNKS_TOTAL: &str = "engram_resume_prefault_chunks_total";

/// Issue #539: histogram of how long each named `[warm]`-hook stage ran,
/// labelled by `stage` (the hook-declared name — cardinality is bounded by
/// however many distinct stage names the fleet's warm hooks emit) and
/// `outcome` (`done` | `failed`). Recorded from `run_warm_hook`'s closed
/// stage history at every terminal point (success, watchdog violation, or
/// non-zero exit) — the productized version of the log-reconstructed
/// "609 s and 1,071 s" / "~33 min" durations the issue's evidence pass had
/// to hand-grep out of host-agent logs.
pub const WARM_HOOK_STAGE_SECONDS: &str = "engram_warm_hook_stage_seconds";

/// Issue #539: counter of `[warm]`-hook capture failures, labelled by
/// `kind` (`engram_core::types::CaptureFailureKind::as_str()` —
/// `warm_exit_non_zero` | `warm_stall` | `warm_stage_deadline` |
/// `warm_global_timeout` | `warm_exec_transport` | `snapshot_failed`).
/// Before this the only signal was the terse `enable_jobs.error` string;
/// this is what tells an operator (or an alert) whether the dominant
/// failure mode is still the stall class after the paired
/// `warm-brain-stack.sh` fix (engrams-internal) lands.
pub const WARM_HOOK_FAILURES_TOTAL: &str = "engram_warm_hook_failures_total";

/// Counter (ADR 0090). One increment per failed heartbeat POST. The
/// fleet alert rule watches the RATE of this per host: sustained
/// nonzero means the host is invisible to the coordinator (node
/// egress/network wedge class — 2026-07-12 incident: 1.5h of lost
/// capacity with only DEBUG traces). Escalating ERROR logs pair with
/// it after ~30s of consecutive failures.
pub const HEARTBEAT_DELIVERY_FAILURES_TOTAL: &str = "engram_host_heartbeat_delivery_failures_total";
/// Counter (ADR 0091). External pause (the rung-2 park's host leg)
/// refused or failed, labeled by bounded `reason`:
/// `capture_in_flight` (typed retryable refusal — the park retries next
/// nomination) / `vmm_pause` (the VMM itself failed the PATCH). Sustained
/// `vmm_pause` means parking is genuinely broken, not merely contended.
pub const RUNG2_PARK_FAILED_TOTAL: &str = "engram_rung2_park_failed_total";

/// Issue #540 (host RAM ledger): gauge of host RAM (MiB) attributed to
/// one named bucket, sampled once per heartbeat tick from
/// `ram_ledger::RamLedgerSnapshot`. Label `category`:
/// - `running_vms` — Σ PSS of reservation-backed (non-parked) FC
///   sandboxes (`RamLedgerSnapshot::running_vm_pss_mib`).
/// - `parked_paused` — Σ PSS of parked-but-resident sandboxes
///   (epic-parking-ladder rungs 2-3; always 0 until the ladder lands).
/// - `base_shm` — measured (`st_blocks`) bytes on the per-image base
///   shm tmpfs.
/// - `base_shm_pending` — registered-but-not-yet-materialized prewarm
///   charges; the reason `allocatable_mib` dips during a prewarm
///   window instead of after it.
/// - `parked_local_memfiles` — NVMe-resident retained memfiles (rung
///   3); disk-side, gauge-only here (chunk-cache-disk-budget owns
///   charging it), always 0 until that ladder rung exists.
///
/// Together these buckets are the attribution `allocatable_mib` never
/// had: every MiB is charged to exactly one category here.
pub const HOST_RAM_LEDGER_MIB: &str = "engram_host_ram_ledger_mib";

/// Issue #540: gauge mirroring `HostUtilization.allocatable_mib` —
/// the same number placement reads off the heartbeat, emitted at the
/// same tick as [`HOST_RAM_LEDGER_MIB`] so the two can never disagree
/// (one snapshot, one emission site).
pub const HOST_RAM_ALLOCATABLE_MIB: &str = "engram_host_ram_allocatable_mib";

/// Issue #540: total capacity (MiB) of the `ENGRAM_FC_UFFD_BASE_DIR`
/// tmpfs — the fixed `uffdBaseTmpfsSize` cap (helm
/// `firecracker.uffdBaseTmpfsSize`, default 32 GiB) node-prep mounts.
/// Surfaces the ceiling the 2026-06-28 `pwrite ... No space left on
/// device` prewarm failure hit, with nothing measuring it beforehand.
pub const HOST_BASE_SHM_TMPFS_TOTAL_MIB: &str = "engram_host_base_shm_tmpfs_total_mib";
/// Issue #540: measured (`st_blocks`) bytes actually allocated on the
/// base-shm tmpfs — companion to
/// [`HOST_BASE_SHM_TMPFS_TOTAL_MIB`] for a used/total ratio.
pub const HOST_BASE_SHM_TMPFS_USED_MIB: &str = "engram_host_base_shm_tmpfs_used_mib";

/// Issue #540: counter incremented each time `image_prefetch`'s
/// base-shm prewarm arm skips the multi-GiB write attempt because the
/// tmpfs headroom pre-check found insufficient free space
/// (`reason="tmpfs_headroom"` — the only reason today, kept as a label
/// for future skip causes). Prewarm's existing warn-and-continue
/// failure posture is unchanged: a skip here still falls back to the
/// handler's lazy per-fault path.
pub const BASE_SHM_PREWARM_SKIPPED_TOTAL: &str = "engram_base_shm_prewarm_skipped_total";

/// Counter (ADR 0075). Populate requests served by the substrate
/// writer. Labels: outcome = already_local | populated | error.
pub const SUBSTRATE_POPULATE_REQUESTS_TOTAL: &str = "engram_substrate_populate_requests_total";

/// R6 (ADR 0098 §Phase 3, #784 layer 1 / #769 gap A): counter incremented
/// each time the startup stale-binding sweep declined to DISCONNECT a
/// dead-owner NBD device because a live process still holds the device node
/// open (`sweep-blocked-live-holder`). The device is left RECONNECTABLE for a
/// later re-serve pass. This should stay at/near zero in steady state; a
/// non-zero value means a survivor's rehydrate was missed upstream (the
/// gap-A recurrence counter) — the same signal as the `sweep-blocked-live-holder`
/// soft-invariant, exported for alerting.
pub const SWEEP_BLOCKED_LIVE_HOLDER_TOTAL: &str = "engram_nbd_sweep_blocked_live_holder_total";

/// Wave 7b (ADR 0098 §Phase 3, #784 layers 2–3): counter incremented each time
/// the startup classification barrier reconciled the kernel-derived NBD
/// inventory against the tracked records and found a CONNECTED device whose live
/// (or unprovable) holder no record could account for — the #769 gap-A survivor,
/// invisible to both the coord-list and the #739 local pass
/// ([`engram_host_core::SlotClass::QuarantinedUnknown`]). The device is parked
/// (left kernel-bound, RECONNECTABLE) and this fires alongside the
/// `rehydrate-unknown-device` soft-invariant. Should stay at zero in steady
/// state; a non-zero value means a survivor's records were lost upstream and an
/// operator/runbook must reconcile it. Distinct from
/// `sweep_blocked_live_holder`: THAT is the per-device sweep declining to sever;
/// THIS is the reconcile finding a device it cannot account for at all.
pub const REHYDRATE_UNKNOWN_DEVICE_TOTAL: &str = "engram_nbd_rehydrate_unknown_device_total";

/// Gauge, label `state` = `capacity` | `free` | `warm` | `in_use` |
/// `parked`. This makes the NBD ceiling and its real high-water pressure
/// visible before a deployment changes `nbds_max` again.
pub const NBD_SLOTS: &str = "engram_nbd_slots";

/// The rehydrate spool-adopt arm found a shutdown spool whose lineage disagrees
/// with the reference disk manifest (fires alongside the
/// `shutdown-spool-lineage-mismatch` soft-invariant). The spool is PRESERVED on
/// disk but its acked writes are not served — the 2026-07-21 61a03b7e incident
/// discarded such a spool because a wrong-kind reference (the MEMORY chain
/// head) made a legitimate spool look foreign. Should stay at zero; non-zero
/// means either a wrong-kind/wrong-lineage reference reached the attach path
/// (a bug) or acked guest writes are sitting unserved (an operator must
/// reconcile).
pub const SPOOL_LINEAGE_MISMATCH_TOTAL: &str = "engram_nbd_spool_lineage_mismatch_total";

/// A SIGTERM shutdown-ladder stage panicked and was unwind-isolated (the
/// ladder continued to the abandon sweep + spool export). Should stay at
/// zero; non-zero means a shutdown rung has a bug — the 2026-08-02
/// durability rollback started as exactly such a panic, silent in prod
/// for 11 days. Alert on any increase.
pub const SHUTDOWN_STAGE_PANIC_TOTAL: &str = "engram_host_shutdown_stage_panic_total";

/// A quarantined survivor's disk was re-served by the rehydrate retry
/// pass (2026-08-02 durability-rollback RCA) — the recovery that
/// replaces the old destroy-on-exhaustion rollback. Informational;
/// the paired failure signal is the coordinator's
/// `engram_quarantine_stuck_total`.
pub const QUARANTINE_REHYDRATE_RECOVERED_TOTAL: &str =
    "engram_nbd_quarantine_rehydrate_recovered_total";

// ─── ADR 0110 rollout gates ───────────────────────────────────────
//
// The dirty file made the write path honest. These four metrics are how
// the rollout is judged; `docs/adr/0110-…md` §Rollout names them. Two
// of them answer "is the new path live", one answers "what did it save",
// one answers "did the honesty cost latency".

/// Counter. Which source seeded a survivor's dirty tier on reattach.
/// Labels: `source` ∈
/// - `dirty_file` — the ADR 0110 path. The file outlived the process and
///   the extent scan rebuilt the dirty set.
/// - `spool` — the pre-0110 fallback. A sandbox created before the
///   upgrade has no dirty file, so the shutdown spool seeds it.
/// - `none` — no dirty file and no adoptable spool. Correct for a
///   sandbox with nothing unpublished; suspicious in a burst.
///
/// **This is the primary rollout gate.** Absence of errors cannot prove
/// the new path runs, because the OLD path is also silent when it works.
/// The first roll after the upgrade reports `spool` for every survivor
/// (no file existed yet); the roll AFTER that is the first real exercise
/// of `dirty_file`. Read as
/// `sum by (source) (increase(engram_nbd_reattach_seed_total[1w]))` —
/// `dirty_file` must climb and `spool` must fall to zero as pre-upgrade
/// sandboxes age out. `spool` still rising after a week means dirty
/// files are not surviving the roll, which is the whole premise failing.
pub const REATTACH_SEED_TOTAL: &str = "engram_nbd_reattach_seed_total";

/// Counter. Extent-scan recoveries, labelled `outcome` ∈ {`ok`,
/// `scan_failed`}. Pairs with [`DIRTY_RECOVERED_BYTES`]: this counts how
/// OFTEN the new recovery ran, that measures how MUCH it saved.
/// `scan_failed` means `SEEK_DATA`/`SEEK_HOLE` errored — the reattach
/// fails and the survivor rides the evict → resume ladder. Alert on any.
pub const DIRTY_RECOVER_TOTAL: &str = "engram_nbd_dirty_recover_total";

/// Histogram. Bytes of allocated extent one extent scan recovered —
/// acked guest writes that a host-agent death would previously have
/// destroyed. This is the ADR's central claim expressed as a number, so
/// it is the evidence for the retirement PR: a week of non-zero samples
/// is proof the write-through path did real work. The `0` bucket splits
/// out the clean-shutdown recoveries that had nothing left to save.
pub const DIRTY_RECOVERED_BYTES: &str = "engram_nbd_dirty_recovered_bytes";

/// Histogram. Seconds one extent scan took. The scan is a synchronous
/// `lseek` loop that runs on the reattach critical path, BEFORE
/// RECONFIGURE releases guest I/O — so a slow scan delays a survivor's
/// resume and is felt by the user. Default `_seconds` buckets are right
/// here: a fast scan collapsing into the 5 ms bucket is fine, because
/// the only question this answers is how bad the slow tail gets.
pub const DIRTY_RECOVER_SECONDS: &str = "engram_nbd_dirty_recover_seconds";

/// Histogram. Seconds from an NBD WRITE reaching the backend to the
/// backend acking it, labelled `outcome` ∈ {`ok`, `eio`}.
///
/// ADR 0110 put a `pwrite` in front of every ack where the RAM map had
/// none, and tradeoff 1 in the ADR is that under memory pressure the
/// kernel can throttle that `pwrite` into writeback. This is the one
/// latency the design newly places on the guest's critical path, and
/// nothing measured it before.
///
/// It carries explicit microsecond-scale buckets (see `init`). The
/// default `_seconds` buckets start at 5 ms, but a `pwrite` into page
/// cache lands in tens of microseconds — every healthy sample would
/// collapse into one bucket and p99 would read "fast" forever.
///
/// Read WITH [`NBD_WRITE_BYTES`]: a p99 rise means nothing on its own,
/// because a workload shift to bigger writes moves it too. A 16 MiB
/// chunk materializing on first touch is the expected slow tail.
pub const NBD_WRITE_ACK_SECONDS: &str = "engram_nbd_write_ack_seconds";

/// Histogram. Bytes per NBD WRITE request. The normalizer for
/// [`NBD_WRITE_ACK_SECONDS`] — it separates "slower because larger"
/// from "slower because the kernel throttled writeback".
pub const NBD_WRITE_BYTES: &str = "engram_nbd_write_bytes";

/// Histogram, label `outcome` (`ok`/`eio`). Wall time of one NBD READ,
/// from dispatch to the backend's reply bytes — the guest-visible serve
/// latency of the chunked rootfs, minus the virtio/kernel-NBD legs.
///
/// This is the north star for the cold-start read-path work (2026-08:
/// a first `gradlew help` is ~47k dependency-chained reads at qd≈1, so
/// wall time = this distribution × chain length). p50 tracks the serve
/// floor; the p90+ tail tracks first-touch whole-chunk materialization.
/// Recorded ONCE per NBD request — a read spanning several chunks must
/// not observe per chunk, both for cost and to keep the distribution
/// per-request.
pub const NBD_READ_SECONDS: &str = "engram_nbd_read_seconds";

/// Histogram. Bytes per NBD READ request. The normalizer for
/// [`NBD_READ_SECONDS`] — guest readahead changes shift the request-size
/// mix, which moves the latency histogram without the path itself
/// changing speed.
pub const NBD_READ_BYTES: &str = "engram_nbd_read_bytes";
