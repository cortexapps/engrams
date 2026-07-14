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

use engram_core::traits::SandboxBackend as _;
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
    /// Issue #529: periodic (ADR 0028 Fix A) vs. an eviction's terminal
    /// snapshot — carried onto the heartbeat advert unchanged so the
    /// reconcile knows whether a fresh row landing warrants a
    /// `SnapshotTaken` emit. `#[serde(default)]` so a record persisted
    /// by a pre-#529 host-agent (mixed roll) reads back as `Periodic`,
    /// its prior sole meaning.
    #[serde(default)]
    pub kind: engram_protocol::heartbeat::CheckpointKind,
}

impl CheckpointRecord {
    pub fn path_in(dir: &Path, id: SnapshotId) -> PathBuf {
        crate::durable_record::record_path(dir, id)
    }

    /// Durably persist (write + fsync via rename) into `dir`.
    pub async fn persist(&self, dir: &Path) -> std::io::Result<()> {
        crate::durable_record::persist(dir, self.snapshot_id, self, "checkpoint record").await
    }

    /// All un-acked records in `dir` (the heartbeat advert payload).
    /// Unreadable/partial files are skipped with a warn — a torn
    /// write must not wedge the heartbeat loop.
    pub async fn load_all(dir: &Path) -> Vec<CheckpointRecord> {
        crate::durable_record::load_all(dir, "checkpoint record").await
    }

    /// Coord acked these — the PG rows own the references now.
    pub async fn delete_acked(dir: &Path, acked: &[SnapshotId]) {
        crate::durable_record::delete_acked(dir, acked.iter().copied(), "checkpoint record").await
    }
}

/// Durable mirror of a sandbox's in-RAM [`CheckpointChain`] head, at
/// `<checkpoint_dir>/chains/<sandbox_id>.json` — the hostPath survives
/// a pod roll, the DashMap does not. Incident 2026-07-13: every VM that
/// survives a host-agent roll (pidfd reattach, ADR 0044 K2 / ADR 0090)
/// lost its chain and paid a FULL multi-GiB memory re-chunk on its next
/// capture; the incident's evict ran 40+ minutes and was killed.
///
/// ## The protocol (why this is NOT just a cache of the chain head)
///
/// FC's `PUT /snapshot/create` — Full and Diff alike — consumes and
/// RESETS the KVM dirty bitmap. A capture that fails after that instant
/// leaves a bitmap baseline nothing durable describes: a diff seeded
/// from any earlier manifest would silently omit the consumed pages
/// (memory corruption on restore — the in-RAM analogue is
/// `poison_checkpoint_chain_after_failed_diff`). So the record is
/// maintained write-ahead:
///
/// 1. **Invalidate** (delete) BEFORE any FC snapshot create;
/// 2. **Persist** only after the chain durably advanced (manifest in
///    the store + in-RAM head updated).
///
/// A crash/SIGKILL anywhere between the two leaves no record, and the
/// rehydrate seeds nothing → the survivor's next capture is a safe
/// Full. Invariant: **record present ⟹ its `manifest_ref` is the
/// durably-published chain head AND no FC snapshot create has run
/// since.** This is also why the coordinator's `snapshots` rows must
/// never be the rehydrate seed source: torn-capture knowledge is
/// host-local (the 2026-07-13 roll tore a diff at 22:23:46, 28 s before
/// the successor pod registered — the latest recoverable row no longer
/// matched the surviving VM's bitmap baseline).
///
/// The unlink needs no dir fsync: losing a completed unlink takes a
/// kernel crash, which also kills the FC VM — and rehydrate only seeds
/// sandboxes that actually reattached, so a resurrected stale record is
/// unreachable and swept by the startup GC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainHeadRecord {
    pub sandbox_id: SandboxId,
    /// The durably-published chain head at persist time.
    pub manifest_ref: ManifestRef,
    /// Bound session, when known — diagnostic only.
    pub session_id: Option<SessionId>,
    pub updated_at: DateTime<Utc>,
}

impl ChainHeadRecord {
    /// The records' directory under the checkpoint root (a sibling of
    /// `records/`).
    pub fn subdir(checkpoint_dir: &Path) -> PathBuf {
        checkpoint_dir.join("chains")
    }

    /// Durably persist (tmp + fsync + rename) — call ONLY after the
    /// in-RAM chain advanced to `manifest_ref` and that manifest is in
    /// the chunk store.
    pub async fn persist(&self, dir: &Path) -> std::io::Result<()> {
        crate::durable_record::persist(dir, self.sandbox_id, self, "chain-head record").await
    }

    /// The record for one sandbox; `None` if absent or torn (a torn
    /// record is treated exactly like a missing one — no seed, next
    /// capture Full).
    pub async fn load(dir: &Path, id: SandboxId) -> Option<ChainHeadRecord> {
        let path = crate::durable_record::record_path(dir, id);
        let bytes = tokio::fs::read(&path).await.ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "unparseable chain-head record; treating as absent");
                None
            }
        }
    }

    /// Every record in `dir` (the startup rehydrate/GC sweep).
    pub async fn load_all(dir: &Path) -> Vec<ChainHeadRecord> {
        crate::durable_record::load_all(dir, "chain-head record").await
    }

    /// Write-ahead invalidate: MUST complete before the FC snapshot
    /// create is issued (see the type doc). Absent is fine; any other
    /// failure must abort the capture — proceeding would leave a record
    /// whose baseline the create is about to consume.
    pub async fn invalidate(dir: &Path, id: SandboxId) -> std::io::Result<()> {
        match tokio::fs::remove_file(crate::durable_record::record_path(dir, id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Best-effort removal for teardown/poison paths (the write-ahead
    /// invalidate already guarantees absence on every capture path; this
    /// is the defensive double-unlink). Sync — a local unlink is
    /// cheaper than the DashMap ops around these call sites.
    pub fn remove_best_effort(dir: &Path, id: SandboxId) {
        let path = crate::durable_record::record_path(dir, id);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e,
                    "chain-head record removal failed");
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
    /// for an unplanned crash of an ACTIVE session: the guest DISK is durable
    /// on a continuous ~30 s flush ([`crate::disk_daemon::flush_scheduler`], no
    /// guest pause) PLUS a final SIGTERM disk-flush pass (issue #225,
    /// [`crate::pooled_backend::PooledBackend::flush_nbd_data_planes_for_shutdown`])
    /// so a routine pod roll loses no acked writes; and the FC memory state is
    /// captured on every event that *quiesces* a session — idle-eviction and
    /// the operator `POST /sessions/:id/snapshot`. NOTE: a SIGTERM pod roll
    /// intentionally does NOT capture FC memory — surviving VMs are detached
    /// and reattached by the successor generation (ADR 0044 K2,
    /// [`crate::lib`]: "no SIGTERM-checkpoint pipeline"); only the disk is
    /// flushed. So the timer bounds how much in-RAM progress an active session
    /// can lose to an *unplanned host crash* (its disk + harness transcript
    /// survive; a planned roll keeps the running VM). 10 min
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
                        // A successful capture proves the control plane
                        // answers — clear any unreachable suspicion.
                        backend.clear_guest_unreachable(sandbox_id);
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
                        // ADR 0091: a checkpoint failure was the ONLY signal a
                        // dead guest emitted, and it died here as a WARN while
                        // the session read `active` (campaign C1: 16+ min
                        // zombie). Confirm with the cheap control-socket probe
                        // — 3 tries, 2s apart, so a mid-restart FC can't be
                        // misclassified — and advertise via the heartbeat.
                        // Only socket-level probe results count: a BUSY guest
                        // fails a capture but still accept()s its API socket.
                        let mut dead_probes = 0u32;
                        for _ in 0..3 {
                            match backend.probe_sandbox(sandbox_id).await {
                                Ok(p) if p.control_alive == Some(false) => dead_probes += 1,
                                _ => break,
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        if dead_probes == 3 {
                            tracing::error!(
                                %sandbox_id,
                                %session_id,
                                "guest control plane is dead (3/3 socket probes refused); \
                                 advertising unreachable (ADR 0091)",
                            );
                            backend.mark_guest_unreachable(sandbox_id, session_id);
                        }
                    }
                }
            }
        }
    }))
}
