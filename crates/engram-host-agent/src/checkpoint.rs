//! ADR 0028 Fix A: periodic coherent (memory, disk) checkpoints.
//!
//! The host is the durable owner of "a coherent snapshot exists in
//! GCS"; the coord's PG row is a cache of that fact. This module holds
//! the pieces around `PooledBackend::checkpoint_sandbox` (which owns
//! the capture pipeline itself):
//!
//! - the sparse-file utility the diff path rides (`dirty_ranges`),
//! - the per-sandbox checkpoint chain state ([`CheckpointChain`]),
//! - the durable, self-describing per-checkpoint record
//!   ([`CheckpointRecord`] — written the moment the upload finishes,
//!   surviving a lost RPC reply / dead coord; re-advertised in every
//!   heartbeat until the coord acks it into PG),
//! - the periodic driver task ([`spawn_checkpoint_driver`]).
//!
//! ## Cost model (why diff-first)
//!
//! `pause → drain NBD flush → FC Diff capture → resume` keeps the
//! guest-visible pause at O(dirty set) — 38 ms measured on a 256 MiB
//! guest vs 2,090 ms for a Full capture (`tests/diff_snapshot.rs`).
//! Everything slow (overlay, incremental re-chunk, upload, record)
//! runs after resume against immutable local staging. At rest the
//! chunk store dedups, so a checkpoint's marginal GCS cost is also
//! O(dirty set); manifests stay full-image chunk lists, so every
//! checkpoint restores with no chain replay.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::types::ids::{SandboxId, SessionId, SnapshotId};
use engram_core::types::manifest::ManifestRef;
use serde::{Deserialize, Serialize};

/// Per-sandbox checkpoint chain state. Lives in
/// `PooledBackend::checkpoint_chains`; seeded manifest-only (no local
/// image) — on resume from the source's memory manifest, or after a
/// fresh Full capture from the just-published one — and advanced by
/// every diff. ADR 0039: always "sparse mode"; the rolling memfile is
/// retired.
pub struct CheckpointChain {
    /// The last published memory manifest — same `manifest_id` for the
    /// chain's lifetime, `version` ticking on every checkpoint.
    pub manifest_ref: ManifestRef,
    /// Its full chunk list — the `prev` for the next incremental
    /// re-chunk. Diffs re-chunk via `update_for_dirty_ranges_sparse`
    /// (fetch the dirty set's prev chunks from the warm cache + apply
    /// the sparse diff), so we never materialize guest RAM — or keep a
    /// full local image — just to checkpoint.
    pub manifest: engram_chunk_store::Manifest,
}

/// Durable, self-describing record of one completed checkpoint.
/// Written to `<records_dir>/<snapshot_id>.json` the moment the
/// upload finishes; deleted when the coord acks it (the PG row now
/// owns the reference). Everything `record_snapshot` needs must be
/// here — the host may be the only survivor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub snapshot_id: SnapshotId,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub image_version: String,
    pub size_bytes: u64,
    pub disk_manifest: Option<ManifestRef>,
    pub memory_manifest: Option<ManifestRef>,
    /// ADR 0035 pins for this checkpoint's device model.
    pub aux_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// The pause instant — the coord resolves the `session_events`
    /// cursor for the A.log coherence triple as "last event at or
    /// before this" when it records the row. (The guest is frozen
    /// from here until resume, so no events it caused can land
    /// after this and still be in the captured state.)
    pub paused_at: DateTime<Utc>,
    pub captured_at: DateTime<Utc>,
}

impl CheckpointRecord {
    pub fn path_in(dir: &Path, id: SnapshotId) -> PathBuf {
        dir.join(format!("{id}.json"))
    }

    /// Durably persist (write + fsync via rename) into `dir`.
    pub async fn persist(&self, dir: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let dest = Self::path_in(dir, self.snapshot_id);
        let tmp = dest.with_extension("json.partial");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serialize checkpoint record: {e}")))?;
        tokio::fs::write(&tmp, &bytes).await?;
        // fsync the temp file so the rename publishes complete bytes.
        let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// All un-acked records in `dir` (the heartbeat advert payload).
    /// Unreadable/partial files are skipped with a warn — a torn
    /// write must not wedge the heartbeat loop.
    pub async fn load_all(dir: &Path) -> Vec<CheckpointRecord> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return out;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read(&p).await {
                Ok(bytes) => match serde_json::from_slice::<CheckpointRecord>(&bytes) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e,
                            "unparseable checkpoint record; skipping");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "unreadable checkpoint record; skipping");
                }
            }
        }
        out
    }

    /// Coord acked these — the PG rows own the references now.
    pub async fn delete_acked(dir: &Path, acked: &[SnapshotId]) {
        for id in acked {
            let p = Self::path_in(dir, *id);
            if let Err(e) = tokio::fs::remove_file(&p).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %p.display(), error = %e,
                        "failed to delete acked checkpoint record");
                }
            }
        }
    }
}

/// Walk `diff`'s data extents (`SEEK_DATA`/`SEEK_HOLE`) and return
/// them as `(offset, len)` ranges. FC's Diff snapshot writes dirty
/// pages at their guest-physical offsets into an otherwise-sparse
/// file, so the extents ARE the dirty set (page-granular, possibly
/// coalesced by the filesystem — coalescing only ever widens, which
/// is safe for the incremental re-chunk).
pub fn dirty_ranges(diff: &Path) -> std::io::Result<Vec<(u64, u64)>> {
    use std::os::unix::io::AsRawFd;

    let src = std::fs::File::open(diff)?;
    let len = src.metadata()?.len() as i64;
    let fd = src.as_raw_fd();
    let mut ranges = Vec::new();
    let mut off: i64 = 0;
    while off < len {
        let data = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
        if data < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() == Some(libc::ENXIO) {
                break; // no more data extents
            }
            return Err(errno);
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        let hole = if hole < 0 { len } else { hole };
        ranges.push((data as u64, (hole - data) as u64));
        off = hole;
    }
    Ok(ranges)
}

/// Config for the periodic driver.
#[derive(Clone, Debug)]
pub struct CheckpointConfig {
    /// Capture cadence per sandbox — the relaxed *in-RAM* backstop (ADR 0043
    /// P2a). `None` disables the periodic driver entirely
    /// (`ENGRAM_CHECKPOINT_INTERVAL_SECS=0`); disk durability and the
    /// event-driven memory checkpoints (drain / idle-evict / operator) are
    /// unaffected either way.
    pub interval: Option<Duration>,
}

impl CheckpointConfig {
    /// ADR 0043 P2a relaxed the default cadence from the old aggressive 60 s
    /// to 10 minutes. The periodic checkpoint is only the *in-RAM* backstop
    /// for an unplanned crash of an ACTIVE session: the guest DISK is already
    /// durable on a continuous ~30 s flush
    /// ([`crate::disk_daemon::flush_scheduler`], no guest pause), and the FC
    /// memory state is captured on every meaningful event — drain (SIGTERM),
    /// idle-eviction, and the operator `POST /sessions/:id/snapshot`. So the
    /// timer only bounds how much in-RAM progress an active session can lose to
    /// an unplanned host crash (its disk + harness transcript survive). 10 min
    /// is "infrequent but sane", with far less per-session pause / capture-lock
    /// contention / GCS churn than every 60 s. Override (or disable, `=0`) via
    /// `ENGRAM_CHECKPOINT_INTERVAL_SECS`.
    pub fn from_env() -> Self {
        let secs = std::env::var("ENGRAM_CHECKPOINT_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);
        Self {
            interval: (secs > 0).then(|| Duration::from_secs(secs)),
        }
    }
}

/// Spawn the periodic checkpoint driver: every `interval`, checkpoint
/// each session-bound sandbox whose last checkpoint is older than the
/// interval. Per-sandbox failures are logged and retried next tick —
/// one wedged guest must not stall the sweep. ADR 0038 B1: a sandbox
/// with a capture already in flight (eviction / evac / drain) is
/// SKIPPED, not queued — a best-effort periodic checkpoint must never
/// stack behind another capture (that gridlocked the fleet).
pub fn spawn_checkpoint_driver(
    backend: Arc<crate::pooled_backend::PooledBackend>,
    cfg: CheckpointConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    let interval = cfg.interval?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick: freshly-created sandboxes get
        // their seed checkpoint one full interval in, by which point
        // agentd is up (and the capture path's wait_agent_ready gate
        // covers the stragglers).
        tick.tick().await;
        loop {
            tick.tick().await;
            let due = backend.checkpoint_candidates(interval);
            for (sandbox_id, session_id) in due {
                // ADR 0038 B1: skip if a capture is already in flight —
                // queuing this best-effort checkpoint behind another
                // capture is what let one slow/hung capture gridlock the
                // fleet. The next tick retries.
                if backend.capture_in_flight(sandbox_id) {
                    metrics::counter!(crate::metrics::CHECKPOINT_SKIPPED_TOTAL).increment(1);
                    tracing::debug!(
                        %sandbox_id,
                        %session_id,
                        "skipping periodic checkpoint; a capture is already in flight",
                    );
                    continue;
                }
                match backend.checkpoint_sandbox(sandbox_id).await {
                    Ok(metadata) => {
                        tracing::info!(
                            %sandbox_id,
                            %session_id,
                            snapshot_id = %metadata.id,
                            memory_manifest = ?metadata.memory_manifest,
                            "periodic checkpoint complete",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            %sandbox_id,
                            %session_id,
                            error = %e,
                            "periodic checkpoint failed; retrying next tick",
                        );
                    }
                }
            }
        }
    }))
}
