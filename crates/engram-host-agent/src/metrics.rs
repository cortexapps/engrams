//! Prometheus metrics exporter for the host-agent.
//!
//! Listens on `ENGRAM_HOST_METRICS_ADDR` (default `0.0.0.0:9100`)
//! so it doubles as the TCP target for the GCE MIG's autohealing
//! health check (see `deploy/terraform/gcp/modules/fc-host-mig/main.tf`
//! — `google_compute_health_check.host_agent` targets this port).
//! Before this listener existed, the MIG marked every instance
//! unhealthy after `initial_delay_sec` and rolled them in a tight
//! loop. Just binding the port satisfies the TCP probe; the
//! metrics themselves are a bonus.
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
///     `notify_session_policy` + the vsock CONNECT + the
///     BootstrapLaunch write):
///     - on the **cold-create** path, emitted from
///       `grpc_server::start_agent` (the coord's gRPC entry).
///       Time is dominated by in-VM boot: kernel + engram-init +
///       ext4 mount + bootstrap binary load before the host's
///       CONNECT succeeds.
///     - on the **warm-lease** path, emitted from
///       `WarmPool::launch`. Same downstream code, but bootstrap
///       is already accept()'ing on the pre-restored microVM, so
///       this should run sub-100ms in the happy case.
///   - `warm_lease` (`WarmPool::lease` — DashMap pop primitive;
///     sub-millisecond in the granted case, slightly longer when
///     the requested template_ref is stale or unknown).
/// - `outcome`: `success` / `invalid_spec` / `fc_error` / for
///   `warm_lease` also `no_capacity` / `stale`.
/// - `kind`: `cold` (sessions that took the full create path) or
///   `warm` (warm-pool-leased sessions). Use this label to compare
///   the same `phase` across the two paths — the headline win of
///   ADR 0014 lands as `agent_handshake{kind="warm"}` being ~25×
///   shorter than `agent_handshake{kind="cold"}`.
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
