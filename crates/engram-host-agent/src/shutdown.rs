//! ADR 0009 Phase 7: SIGTERM checkpoint pipeline.
//!
//! Installed by the host-agent's runtime at startup when
//! `ENGRAM_GRACEFUL_SHUTDOWN=1`. On SIGTERM or SIGINT:
//!
//! 1. Set the shutdown flag (refuse new `create()` requests; not
//!    wired in this phase — TODO post-Phase-7).
//! 2. Drain in-flight RPCs (`--shutdown-drain-secs`, default 5 s).
//! 3. For each live sandbox in parallel
//!    (`--shutdown-checkpoint-parallelism`, default 8), call the
//!    backend's `snapshot()` (which on the wrapped `PooledBackend`
//!    chunks memory + disk into the chunk store). Update the FC
//!    sandbox manifest's `last_local_snapshot` field with the
//!    resulting `(disk_manifest, memory_manifest)` refs so the
//!    Phase 8 reattach pass can fall through to NVMe restore.
//! 4. Exit cleanly. If we miss `--shutdown-deadline-secs` (default
//!    25 s), exit anyway — sandboxes that didn't checkpoint will
//!    flip per reconcile §3.
//!
//! "Local NVMe" qualifier: the chunk store writes through whatever
//! `BlobStorage` is wired (GCS in prod, LocalBlobStorage in dev).
//! The seconds-scale budget assumes either (a) `LocalBlobStorage`
//! for genuinely-local writes, or (b) GCS with chunked-memory dedup
//! reducing per-sandbox upload to a handful of dirty chunks. The
//! Phase 8 reattach reads via the same store, so the path closes
//! regardless of which BlobStorage backs it — the local-only
//! optimization is a future tuning knob, not a correctness gate.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::SandboxBackend;
use engram_core::SandboxId;
use tokio::sync::Semaphore;

/// Default deadline for the full SIGTERM-to-exit window. Matches
/// systemd's default `TimeoutStopSec=90s` minus margin, and fits
/// inside the 25 s GCE Spot preemption notice. Override via
/// `--shutdown-deadline-secs` / `ENGRAM_SHUTDOWN_DEADLINE_SECS`.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(25);

/// Default time to wait for in-flight `exec_stream` / `snapshot` /
/// `restore` RPCs to finish before starting the checkpoint pipeline.
/// 5 s is enough for normal RPCs to drain; longer ones are aborted
/// by the deadline budget anyway.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);

/// Default parallelism for the per-sandbox checkpoint fan-out.
/// 8 strikes a balance between (a) wall-clock budget — 50 sandboxes
/// at 500 ms each parallelized 8-way is ~3 s — and (b) NVMe / GCS
/// contention. Tune per host.
pub const DEFAULT_PARALLELISM: usize = 8;

/// Knobs read at startup. Override via env or future CLI flags.
#[derive(Clone, Debug)]
pub struct ShutdownConfig {
    pub enabled: bool,
    pub deadline: Duration,
    pub drain: Duration,
    pub parallelism: usize,
}

impl ShutdownConfig {
    /// Read knobs from env. `ENGRAM_GRACEFUL_SHUTDOWN=1` enables;
    /// other vars override the timing defaults. Falls through to
    /// `disabled` if the gate var isn't set, so existing operators
    /// see no behaviour change until they opt in.
    pub fn from_env() -> Self {
        let enabled = std::env::var("ENGRAM_GRACEFUL_SHUTDOWN").ok().as_deref() == Some("1");
        let deadline = parse_secs("ENGRAM_SHUTDOWN_DEADLINE_SECS").unwrap_or(DEFAULT_DEADLINE);
        let drain = parse_secs("ENGRAM_SHUTDOWN_DRAIN_SECS").unwrap_or(DEFAULT_DRAIN);
        let parallelism = std::env::var("ENGRAM_SHUTDOWN_CHECKPOINT_PARALLELISM")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(DEFAULT_PARALLELISM);
        Self {
            enabled,
            deadline,
            drain,
            parallelism,
        }
    }
}

fn parse_secs(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Install the SIGTERM + SIGINT handler. Returns a future that
/// completes when either signal arrives. Caller `tokio::select!`s
/// this against the dialer's `run_dialer` loop so SIGTERM is
/// observed promptly and the checkpoint pipeline runs to completion
/// before the process exits.
///
/// On non-Linux (no SIGTERM concept that matters for this design)
/// the future never resolves — host-agent restart on macOS-style
/// shutdown isn't the case-C/C' scenario we're targeting.
pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler install failed; graceful shutdown won't trigger");
                std::future::pending::<()>().await;
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGINT handler install failed; will only trigger on SIGTERM");
                sigterm.recv().await;
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM; entering graceful shutdown"),
            _ = sigint.recv() => tracing::info!("received SIGINT; entering graceful shutdown"),
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}

/// Drive the graceful-shutdown pipeline: drain → checkpoint fan-out.
/// Bounded by `cfg.deadline`. Returns when either every sandbox
/// has checkpointed or the deadline elapses — the host-agent then
/// exits cleanly.
///
/// `work_dir` is the FC `work_dir` so we can update each sandbox's
/// `sandbox.json` with `last_local_snapshot` post-checkpoint.
pub async fn run(
    cfg: &ShutdownConfig,
    backend: Arc<dyn SandboxBackend>,
    work_dir: PathBuf,
) -> ShutdownStats {
    let mut stats = ShutdownStats::default();
    if !cfg.enabled {
        tracing::info!("graceful shutdown disabled (ENGRAM_GRACEFUL_SHUTDOWN not set); exiting");
        return stats;
    }
    let start = std::time::Instant::now();
    let deadline = start + cfg.deadline;

    // Step 1: drain. Sleep for `cfg.drain` so in-flight RPCs have a
    // moment to finish on their own. Future enhancement: a real
    // refcount of in-flight ops + cancellation tokens. The plain
    // sleep is enough for the seconds-scale workloads we're
    // targeting.
    tracing::info!(drain_secs = ?cfg.drain, "draining in-flight RPCs");
    tokio::time::sleep(cfg.drain).await;

    // Step 2: enumerate live sandboxes + fan out the checkpoint.
    let sandbox_ids = match backend.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "backend.list() failed during shutdown; nothing to checkpoint");
            return stats;
        }
    };
    stats.total = sandbox_ids.len();
    if sandbox_ids.is_empty() {
        tracing::info!("no live sandboxes; exiting");
        return stats;
    }
    tracing::info!(
        total = stats.total,
        parallelism = cfg.parallelism,
        "starting checkpoint fan-out"
    );

    let sem = Arc::new(Semaphore::new(cfg.parallelism));
    let mut handles = Vec::with_capacity(sandbox_ids.len());
    for id in sandbox_ids {
        let permit = sem.clone();
        let backend = backend.clone();
        let work_dir = work_dir.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit.acquire_owned().await.ok()?;
            checkpoint_one(backend, id, work_dir).await
        }));
    }

    // Step 3: await all handles, bounded by deadline. Any handle
    // still running when the deadline fires is dropped — the host-
    // agent process will exit shortly after this function returns.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now().into_std());
    let drain = tokio::time::timeout(remaining, async {
        for h in handles {
            match h.await {
                Ok(Some(true)) => stats.checkpointed += 1,
                Ok(Some(false)) => stats.failed += 1,
                Ok(None) => stats.failed += 1,
                Err(e) => {
                    tracing::warn!(error = %e, "checkpoint task panicked");
                    stats.failed += 1;
                }
            }
        }
    })
    .await;
    if drain.is_err() {
        stats.timed_out = stats
            .total
            .saturating_sub(stats.checkpointed + stats.failed);
        tracing::warn!(
            deadline_secs = cfg.deadline.as_secs(),
            timed_out = stats.timed_out,
            "graceful shutdown deadline exceeded; some sandboxes did not checkpoint"
        );
    }

    tracing::info!(
        checkpointed = stats.checkpointed,
        failed = stats.failed,
        timed_out = stats.timed_out,
        elapsed_secs = start.elapsed().as_secs_f64(),
        "graceful shutdown complete"
    );
    stats
}

/// Per-sandbox checkpoint. Calls `backend.snapshot()` (which on the
/// PooledBackend chunks memory + disk), then updates the sandbox's
/// on-disk manifest with `last_local_snapshot` so Phase 8's reattach
/// can find it. Returns `true` on success, `false` on any failure.
async fn checkpoint_one(
    backend: Arc<dyn SandboxBackend>,
    id: SandboxId,
    work_dir: PathBuf,
) -> Option<bool> {
    let metadata = match backend.snapshot(id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(sandbox_id = %id, error = %e, "shutdown checkpoint snapshot() failed");
            return Some(false);
        }
    };

    // Update sandbox.json with `last_local_snapshot` pointing at
    // the just-taken manifests. Best-effort: failure here means
    // the snapshot is in the chunk store but Phase 8 reattach
    // won't know to use it (reconcile flips the session per §3).
    let manifest_path = engram_sandbox_firecracker::sandbox_manifest::manifest_path(&work_dir, id);
    let mut manifest =
        match engram_sandbox_firecracker::sandbox_manifest::read_manifest(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "shutdown checkpoint: sandbox.json missing or unreadable; \
                     snapshot taken but not registered for Phase 8 reattach"
                );
                return Some(true);
            }
        };
    manifest.last_local_snapshot = Some(
        engram_sandbox_firecracker::sandbox_manifest::LocalSnapshotRef {
            snapshot_id: Some(metadata.id),
            disk_manifest_id: metadata
                .disk_manifest
                .as_ref()
                .map(|m| m.manifest_id)
                .unwrap_or_default(),
            disk_manifest_version: metadata
                .disk_manifest
                .as_ref()
                .map(|m| m.version)
                .unwrap_or(0),
            memory_manifest_id: metadata.memory_manifest.as_ref().map(|m| m.manifest_id),
            memory_manifest_version: metadata.memory_manifest.as_ref().map(|m| m.version),
            taken_at: chrono::Utc::now(),
            trigger: "sigterm".into(),
        },
    );
    if let Err(e) =
        engram_sandbox_firecracker::sandbox_manifest::write_manifest(&manifest_path, &manifest)
    {
        tracing::warn!(
            sandbox_id = %id,
            error = %e,
            "shutdown checkpoint: manifest update failed; Phase 8 reattach will skip this sandbox"
        );
    } else {
        tracing::info!(
            sandbox_id = %id,
            disk_manifest = ?metadata.disk_manifest,
            memory_manifest = ?metadata.memory_manifest,
            "shutdown checkpoint succeeded (Phase 7 / case C')"
        );
    }
    Some(true)
}

#[derive(Debug, Default)]
pub struct ShutdownStats {
    pub total: usize,
    pub checkpointed: usize,
    pub failed: usize,
    pub timed_out: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// Cargo runs tests in parallel by default; `ShutdownConfig::from_env`
    /// reads process-global state, so concurrent test threads race
    /// on the same env vars. Serialize via a module-static mutex —
    /// each test acquires before touching env and the guard drops
    /// on scope exit.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        std::env::remove_var("ENGRAM_GRACEFUL_SHUTDOWN");
        std::env::remove_var("ENGRAM_SHUTDOWN_DEADLINE_SECS");
        std::env::remove_var("ENGRAM_SHUTDOWN_DRAIN_SECS");
        std::env::remove_var("ENGRAM_SHUTDOWN_CHECKPOINT_PARALLELISM");
    }

    #[test]
    fn from_env_defaults_when_unset() {
        let _g = ENV_LOCK.lock();
        clear_env();
        let cfg = ShutdownConfig::from_env();
        assert!(!cfg.enabled, "must default to disabled (opt-in only)");
        assert_eq!(cfg.deadline, DEFAULT_DEADLINE);
        assert_eq!(cfg.drain, DEFAULT_DRAIN);
        assert_eq!(cfg.parallelism, DEFAULT_PARALLELISM);
    }

    #[test]
    fn from_env_respects_overrides() {
        let _g = ENV_LOCK.lock();
        clear_env();
        std::env::set_var("ENGRAM_GRACEFUL_SHUTDOWN", "1");
        std::env::set_var("ENGRAM_SHUTDOWN_DEADLINE_SECS", "60");
        std::env::set_var("ENGRAM_SHUTDOWN_DRAIN_SECS", "2");
        std::env::set_var("ENGRAM_SHUTDOWN_CHECKPOINT_PARALLELISM", "4");
        let cfg = ShutdownConfig::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.deadline, Duration::from_secs(60));
        assert_eq!(cfg.drain, Duration::from_secs(2));
        assert_eq!(cfg.parallelism, 4);
        clear_env();
    }

    #[test]
    fn parallelism_zero_clamps_to_one() {
        let _g = ENV_LOCK.lock();
        clear_env();
        std::env::set_var("ENGRAM_SHUTDOWN_CHECKPOINT_PARALLELISM", "0");
        let cfg = ShutdownConfig::from_env();
        assert_eq!(cfg.parallelism, 1);
        clear_env();
    }
}
