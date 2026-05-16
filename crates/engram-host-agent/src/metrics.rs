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

/// Histogram. Wall-clock time the `SandboxBackend::create` call
/// took on this host. Labels:
/// - `phase`: cold-path sub-step name. Emitted today:
///   - `image_resolve` (sum of `ensure_image` + `ensure_harness_ext4`
///     — both are OCI/cache lookups)
///   - `materialize` (`resolve_rootfs` — NBD daemon spawn or
///     materialize-to-file for chunked rootfs)
///   - `fc_boot` (`FirecrackerBackend::create` — FC API
///     PUT-boot-source / PUT-drive / PUT-vsock / InstanceStart)
///   - `agent_handshake` (host blocks on the in-guest bootstrap
///     accept()'ing on vsock 1025; this is the in-VM-boot phase
///     where kernel + engram-init + ext4 mount + bootstrap binary
///     load happen before the host's CONNECT succeeds)
///   - `create_total` (pooled_backend's full create — sum of the
///     first three; `agent_handshake` is a separate gRPC call)
/// - `outcome`: `success` / `invalid_spec` / `fc_error`.
/// - `kind`: `cold` (the only value today; warm-path emissions
///   come from `WarmPool::lease`/`launch` and will land in a
///   follow-up).
///
/// Pairs with the coord-side `engram_session_boot_seconds` — same
/// operation, different vantage; comparing the two surfaces gRPC
/// round-trip overhead. Drill-down dashboard query example:
/// `histogram_quantile(0.95, sum by (phase, le) (rate(
/// engram_sandbox_boot_seconds_bucket{kind="cold"}[5m])))`.
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
