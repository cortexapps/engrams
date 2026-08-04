//! #1003: jemalloc heap profiling + allocator observability.
//!
//! The prod host-agent is musl-static. Under musl's mallocng, the
//! chunk-buffer churn (16 MiB flush claim copies, memory re-chunk
//! buffers) pinned hundreds of ~48 MiB allocation groups — 23 GB of
//! an idle process — and `/proc` cannot attribute heap to call sites.
//! jemalloc gives us both halves:
//!
//! - **Attribution**: a sampled profiler (`lg_prof_sample:19` ≈ one
//!   sample per 512 KiB allocated — negligible overhead) with an
//!   on-demand dump: `SIGUSR2` writes a symbolized pprof profile to
//!   the dump dir and logs the path. Retrieve with `kubectl cp`,
//!   inspect with `pprof`/`go tool pprof`.
//! - **Continuous observability**: allocator stats exported as
//!   Prometheus gauges every 30 s, so retention/ratchet behavior is a
//!   dashboard line instead of cgroup archaeology:
//!   `engram_host_allocator_{allocated,active,resident,mapped,retained}_bytes`.
//!
//! Everything here is Linux-only (gated with the deps); macOS dev
//! builds keep the default allocator.

use std::path::PathBuf;

/// Spawn the SIGUSR2 dump listener and the stats-gauge loop. Called
/// once from `main` after the metrics exporter is up.
pub fn spawn(dump_dir: PathBuf) {
    tokio::spawn(async move {
        let mut sig =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "heap-profile: SIGUSR2 handler install failed");
                    return;
                }
            };
        loop {
            sig.recv().await;
            match dump_pprof().await {
                Ok(bytes) => {
                    let path = dump_dir.join(format!(
                        "engram-heap-{}.pb.gz",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                    ));
                    match tokio::fs::write(&path, &bytes).await {
                        Ok(()) => tracing::info!(
                            path = %path.display(),
                            bytes = bytes.len(),
                            "heap-profile: pprof dump written (retrieve with kubectl cp)",
                        ),
                        Err(e) => tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "heap-profile: dump write failed",
                        ),
                    }
                }
                Err(e) => tracing::warn!(error = %e, "heap-profile: pprof dump failed"),
            }
        }
    });

    tokio::spawn(async {
        loop {
            export_stats();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

async fn dump_pprof() -> Result<Vec<u8>, String> {
    let mut ctl = jemalloc_pprof::PROF_CTL
        .as_ref()
        .ok_or_else(|| "profiling not compiled/enabled (check _rjem_malloc_conf)".to_string())?
        .lock()
        .await;
    if !ctl.activated() {
        ctl.activate().map_err(|e| e.to_string())?;
    }
    ctl.dump_pprof().map_err(|e| e.to_string())
}

/// Refresh jemalloc's stats epoch and export the headline numbers.
/// `retained` is the one that names the #1003 ratchet: virtual memory
/// jemalloc holds after freeing (decayed but not unmapped). A
/// `resident`-vs-`allocated` gap that grows without bound is the
/// retention signature; `allocated` growing alone is a live leak.
fn export_stats() {
    if tikv_jemalloc_ctl::epoch::advance().is_err() {
        return;
    }
    let read = |name: &'static str, value: Result<usize, tikv_jemalloc_ctl::Error>| {
        if let Ok(v) = value {
            ::metrics::gauge!(name).set(v as f64);
        }
    };
    read(
        "engram_host_allocator_allocated_bytes",
        tikv_jemalloc_ctl::stats::allocated::read(),
    );
    read(
        "engram_host_allocator_active_bytes",
        tikv_jemalloc_ctl::stats::active::read(),
    );
    read(
        "engram_host_allocator_resident_bytes",
        tikv_jemalloc_ctl::stats::resident::read(),
    );
    read(
        "engram_host_allocator_mapped_bytes",
        tikv_jemalloc_ctl::stats::mapped::read(),
    );
    read(
        "engram_host_allocator_retained_bytes",
        tikv_jemalloc_ctl::stats::retained::read(),
    );
}
