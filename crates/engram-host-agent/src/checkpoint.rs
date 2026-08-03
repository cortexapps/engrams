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
use engram_host_core::{HostFs, TokioFs};
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

    /// Durably persist (write + fsync via rename) into `dir`, through the
    /// injected fs seam (ADR 0098 P5 — Flow D's terminal record crosses
    /// it; other flows pass [`TokioFs`]).
    pub async fn persist(&self, fs: &dyn HostFs, dir: &Path) -> std::io::Result<()> {
        crate::durable_record::persist(fs, dir, self.snapshot_id, self, "checkpoint record").await
    }

    /// All un-acked records in `dir` (the heartbeat advert payload).
    /// Unreadable/partial files are skipped with a warn — a torn
    /// write must not wedge the heartbeat loop.
    pub async fn load_all(fs: &dyn HostFs, dir: &Path) -> Vec<CheckpointRecord> {
        crate::durable_record::load_all(fs, dir, "checkpoint record").await
    }

    /// Coord acked these — the PG rows own the references now.
    pub async fn delete_acked(fs: &dyn HostFs, dir: &Path, acked: &[SnapshotId]) {
        crate::durable_record::delete_acked(fs, dir, acked.iter().copied(), "checkpoint record")
            .await
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
///
/// All mutation goes through [`ChainHeadStore`] (never bare file ops) —
/// see its doc for the cancellation-ordering guarantee.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainHeadRecord {
    pub sandbox_id: SandboxId,
    /// The durably-published chain head at persist time.
    pub manifest_ref: ManifestRef,
    /// Bound session, when known. Load-bearing since the local
    /// survivor-rehydrate pass (session 731df805, 2026-07-17):
    /// `PooledBackend::rehydrate_local_survivors` re-serves a live
    /// survivor's NBD device under this session when the coordinator's
    /// register-time list misses it. `None` (a record persisted before
    /// the binding was known) exempts the sandbox from the local pass.
    pub session_id: Option<SessionId>,
    pub updated_at: DateTime<Utc>,
}

impl ChainHeadRecord {
    /// The records' directory under the checkpoint root (a sibling of
    /// `records/`).
    pub fn subdir(checkpoint_dir: &Path) -> PathBuf {
        checkpoint_dir.join("chains")
    }

    /// The record for one sandbox; `None` if absent or torn (a torn
    /// record is treated exactly like a missing one — no seed, next
    /// capture Full).
    pub async fn load(dir: &Path, id: SandboxId) -> Option<ChainHeadRecord> {
        let path = crate::durable_record::record_path(dir, id);
        let bytes = tokio::fs::read(&path).await.ok()?;
        // R5: open the sealed envelope (content hash + sandbox-id identity); a
        // torn/bit-rotted/misdirected record is treated as absent, same as the
        // pre-envelope unparseable arm — the chain seeds Full next capture.
        let body = match crate::durable_envelope::open(&bytes, &id.to_string()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "corrupt chain-head record; treating as absent");
                return None;
            }
        };
        match serde_json::from_slice(&body) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "unparseable chain-head record body; treating as absent");
                None
            }
        }
    }

    /// Every record in `dir` (the startup rehydrate/GC sweep). Not yet
    /// behind the fs seam — the chain-head flow extracts in a later P
    /// (the seam lands with the flows that cross it).
    pub async fn load_all(dir: &Path) -> Vec<ChainHeadRecord> {
        crate::durable_record::load_all(&TokioFs, dir, "chain-head record").await
    }
}

/// Owner of every [`ChainHeadRecord`] MUTATION, closing the detached-
/// tail cancellation race: `tokio::fs`/`spawn_blocking` operations keep
/// running after their awaiting future is cancelled, and cancellation
/// also releases the capture lock — so a cancelled `persist`'s rename
/// could land AFTER a later capture's write-ahead invalidate,
/// resurrecting a record whose baseline that capture's FC create just
/// consumed (the corruption the write-ahead protocol exists to
/// prevent).
///
/// The guard: a per-sandbox **epoch** bumped by every invalidate,
/// atomically with the unlink, under a per-sandbox mutex. A persist
/// captures the epoch when it is INITIATED (under the capture lock,
/// after its own capture's invalidate) and re-checks it under the same
/// mutex immediately before the rename — a detached tail that lost the
/// race to a newer invalidate refuses to publish, in either interleaving:
///
/// - tail publishes first → the newer invalidate's unlink removes it;
/// - the newer invalidate runs first → the tail's epoch check fails.
///
/// Epochs are in-process state, which is sufficient: a dead process's
/// detached tails die with it, and cross-process ordering is what the
/// on-disk write-ahead protocol itself provides. Per-sandbox states are
/// kept for the process lifetime (bytes each; a removed entry could
/// let a detached tail race a fresh one).
pub struct ChainHeadStore {
    dir: PathBuf,
    states: dashmap::DashMap<SandboxId, Arc<RecState>>,
}

#[derive(Default)]
struct RecState {
    /// Bumped (under `io`) by every invalidate; a persist initiated
    /// before the bump refuses to publish.
    epoch: std::sync::atomic::AtomicU64,
    /// Serializes the epoch-check+rename / bump+unlink transactions.
    io: std::sync::Mutex<()>,
}

/// Collision-free temp names across a live persist and a detached tail
/// for the same sandbox (a shared temp path would let the tail's
/// cleanup delete the live persist's staged bytes).
static PERSIST_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ChainHeadStore {
    pub fn new(checkpoint_dir: &Path) -> Self {
        Self {
            dir: ChainHeadRecord::subdir(checkpoint_dir),
            states: dashmap::DashMap::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn state(&self, id: SandboxId) -> Arc<RecState> {
        self.states.entry(id).or_default().clone()
    }

    /// Write-ahead invalidate: MUST complete before the FC snapshot
    /// create is issued (see [`ChainHeadRecord`]'s doc). Sync and
    /// inline — one unlink, no await point a cancellation could split.
    /// Absent is fine; any other failure must abort the capture —
    /// proceeding would leave a record whose baseline the create is
    /// about to consume.
    pub fn invalidate(&self, id: SandboxId) -> std::io::Result<()> {
        let st = self.state(id);
        let _g = st.io.lock().expect("chain-head io lock poisoned");
        st.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match std::fs::remove_file(crate::durable_record::record_path(&self.dir, id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Best-effort invalidate for teardown/poison paths (the capture
    /// paths' write-ahead invalidate already guarantees absence; this
    /// is the defensive double-unlink — it still bumps the epoch, so it
    /// also fences any straggling persist).
    pub fn remove_best_effort(&self, id: SandboxId) {
        if let Err(e) = self.invalidate(id) {
            tracing::warn!(sandbox_id = %id, error = %e,
                "chain-head record removal failed");
        }
    }

    /// Durably persist (tmp + fsync + epoch-checked rename + dir fsync)
    /// — call ONLY after the in-RAM chain advanced to the record's
    /// `manifest_ref` and that manifest is in the chunk store. The whole
    /// transaction runs in one `spawn_blocking`; if the awaiting future
    /// is cancelled the detached transaction still either publishes
    /// atomically (and a subsequent invalidate removes it) or refuses
    /// because the epoch moved. `Ok` includes the superseded no-op.
    pub async fn persist(&self, record: ChainHeadRecord) -> std::io::Result<()> {
        let st = self.state(record.sandbox_id);
        let epoch0 = st.epoch.load(std::sync::atomic::Ordering::SeqCst);
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || Self::persist_at_epoch(&dir, &st, record, epoch0))
            .await
            .map_err(|e| std::io::Error::other(format!("chain-head persist join: {e}")))?
    }

    /// The blocking transaction body; `epoch0` is the epoch observed
    /// when the persist was initiated. Split out (and epoch-explicit)
    /// so the detached-tail interleaving is deterministically testable.
    fn persist_at_epoch(
        dir: &Path,
        st: &RecState,
        record: ChainHeadRecord,
        epoch0: u64,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let dest = crate::durable_record::record_path(dir, record.sandbox_id);
        let nonce = PERSIST_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dest.with_extension(format!("json.partial.{nonce}"));
        // R5: seal the record under a content hash + the sandbox id, so its
        // custom epoch-checked write matches the durable_record envelope its
        // `load`/`load_all` now expect (chain-head persists here, not through
        // durable_record::persist, for the epoch-fenced rename).
        let body = serde_json::to_string_pretty(&record)
            .map_err(|e| std::io::Error::other(format!("serialize chain-head record: {e}")))?;
        let bytes = crate::durable_envelope::seal(&record.sandbox_id.to_string(), &body);
        std::fs::write(&tmp, &bytes)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        {
            let _g = st.io.lock().expect("chain-head io lock poisoned");
            if !engram_host_core::checkpoint_tail_admits_publish(
                epoch0,
                st.epoch.load(std::sync::atomic::Ordering::SeqCst),
            ) {
                // A newer invalidate fenced this persist off — its
                // record describes a baseline an FC create has since
                // consumed. Publishing it would be the resurrection
                // this store exists to prevent.
                let _ = std::fs::remove_file(&tmp);
                tracing::debug!(
                    sandbox_id = %record.sandbox_id,
                    "chain-head persist superseded by a newer invalidate; not published",
                );
                return Ok(());
            }
            std::fs::rename(&tmp, &dest)?;
        }
        // Dir fsync so the rename itself is crash-durable (matches
        // durable_record::persist).
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }

    /// Test-only view of a sandbox's current epoch (to simulate a
    /// detached persist tail deterministically).
    #[cfg(test)]
    fn epoch_for_test(&self, id: SandboxId) -> u64 {
        self.state(id)
            .epoch
            .load(std::sync::atomic::Ordering::SeqCst)
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
    /// The MAX epoch length — the age backstop every session-bound
    /// sandbox is checkpointed at regardless of activity (the relaxed
    /// *in-RAM* backstop, ADR 0043 P2a). `None` disables the periodic
    /// driver entirely (`ENGRAM_CHECKPOINT_INTERVAL_SECS=0`); disk
    /// durability and the event-driven memory checkpoints (drain /
    /// idle-evict / operator) are unaffected either way.
    pub interval: Option<Duration>,
    /// ADR 0101 B: the MIN epoch length — the adaptive controller never
    /// checkpoints a sandbox more often than this, and it is the
    /// driver's scheduling quantum. `ENGRAM_CHECKPOINT_MIN_INTERVAL_SECS`.
    pub min_interval: Duration,
    /// ADR 0101 B: how much memory dirt one epoch should aim to carry.
    /// The controller scales the next epoch so `dirty_bytes ≈ target` at
    /// the last observed dirty rate. `ENGRAM_CHECKPOINT_TARGET_EPOCH_MB`.
    pub target_epoch_bytes: u64,
}

/// ADR 0101 B: the next epoch length, from the last epoch's observed
/// dirty rate. Aim for `target_epoch_bytes` of dirt per capture:
/// `next = last_epoch × target / last_dirty`, clamped to
/// `[min_interval, max_interval]`. No rate signal (first capture after
/// a chain seed, a Full capture, a zero-dirty epoch) → the max
/// backstop — an idle guest keeps the cheap ADR 0043 cadence; only a
/// guest actually dirtying RAM earns short epochs. Pure — unit-tested
/// directly, and the driver stays simulable (ADR 0098).
pub fn next_epoch_after(
    last_epoch: Duration,
    last_dirty_bytes: Option<u64>,
    min_interval: Duration,
    max_interval: Duration,
    target_epoch_bytes: u64,
) -> Duration {
    // Order-safe (engrams review, #835): `f64::clamp` PANICS when
    // min > max, and nothing upstream forbids an operator setting
    // `ENGRAM_CHECKPOINT_INTERVAL_SECS` below the 30s MIN default (a
    // natural way to ask for more frequent checkpoints). This runs
    // inside the un-awaited driver task, where a panic silently kills
    // all periodic checkpoints for the host — degrade to the max bound
    // instead (the operator lowered the ceiling; honor it). `from_env`
    // also normalizes the pair, so this guard is belt-and-suspenders
    // for direct-constructed configs.
    let max = max_interval;
    let min = min_interval.min(max);
    let Some(dirty) = last_dirty_bytes else {
        return max;
    };
    if dirty == 0 || last_epoch.is_zero() || target_epoch_bytes == 0 {
        return max;
    }
    let scaled = last_epoch.as_secs_f64() * (target_epoch_bytes as f64) / (dirty as f64);
    Duration::from_secs_f64(scaled.clamp(min.as_secs_f64(), max.as_secs_f64()))
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
    /// flushed, new captures are refused (the quiesce gate in
    /// `capture_phase`), and an in-flight capture is drained bounded (the
    /// CaptureDrain ladder stage) so its consumed dirty bitmap never dies
    /// with the process. So the timer bounds how much in-RAM progress an active session
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
        // ADR 0101 B: Phase A made small diffs cheap (unchecked PUTs,
        // same-hash skip, 96-way fan-out), so busy sessions can afford
        // short epochs again — adaptively, not the old flat 60s that
        // ADR 0043 P2a retired. 30s floor; 256 MiB dirt per epoch
        // target (~1-6s of finalize work post-Phase-A).
        let min_secs = std::env::var("ENGRAM_CHECKPOINT_MIN_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let target_mb = std::env::var("ENGRAM_CHECKPOINT_TARGET_EPOCH_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(256);
        // Normalize a min > max pair (engrams review, #835): an operator
        // lowering INTERVAL below the MIN default asked for a tighter
        // ceiling — honor it rather than hand `next_epoch_after` an
        // inverted clamp. Loud, because the MIN knob is being ignored.
        let mut min_secs = min_secs.max(1);
        if secs > 0 && min_secs > secs {
            tracing::warn!(
                min_secs,
                interval_secs = secs,
                "ENGRAM_CHECKPOINT_MIN_INTERVAL_SECS exceeds ENGRAM_CHECKPOINT_INTERVAL_SECS; \
                 clamping the floor to the ceiling",
            );
            min_secs = secs;
        }
        Self {
            interval: (secs > 0).then(|| Duration::from_secs(secs)),
            min_interval: Duration::from_secs(min_secs),
            target_epoch_bytes: target_mb * 1024 * 1024,
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
    cfg.interval?;
    Some(tokio::spawn(async move {
        // ADR 0101 B: tick at the MIN interval — the scheduling quantum.
        // Which sandboxes are actually due is decided per-sandbox by the
        // adaptive controller (`checkpoint_candidates_adaptive`), so an
        // idle fleet still captures only every `interval` (the max
        // backstop); the fast quantum exists so a busy sandbox's short
        // epoch is honored. The candidate scan is a pure in-RAM map
        // walk — waking it every `min_interval` costs nothing.
        let mut tick = tokio::time::interval(cfg.min_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick: freshly-created sandboxes get
        // their seed checkpoint later, by which point agentd is up (and
        // the capture path's wait_agent_ready gate covers stragglers).
        tick.tick().await;
        loop {
            tick.tick().await;
            run_checkpoint_pass(&backend, &cfg).await;
        }
    }))
}

/// One sleep-free pass of the periodic driver — the ADR 0098
/// `spawn()`/`run_once()` split: the timer loop above is a thin
/// wrapper, and this is the step tests (and the host simulator) drive
/// directly. Genuinely sleep-free (engrams review, #835): the ADR 0091
/// dead-guest confirmation probe (3 tries, 2s apart) is SPAWNED
/// detached, not awaited inline — one unresponsive guest must not
/// consume the `min_interval` quantum and head-of-line the honored
/// short epochs of the busy sandboxes behind it.
pub async fn run_checkpoint_pass(
    backend: &Arc<crate::pooled_backend::PooledBackend>,
    cfg: &CheckpointConfig,
) {
    // 2026-08-03 `chain_poisoned` alert: SIGTERM raised the capture
    // quiesce — a capture started now would race the process teardown
    // and poison its chain after FC consumed the dirty bitmap. Skip the
    // whole pass (quietly: `capture_phase`'s own gate is the
    // authoritative refusal; this early-out just avoids a WARN + dead-
    // guest probe per sandbox on every tick of the shutdown window).
    if backend.captures_quiesced() {
        tracing::debug!("periodic checkpoint pass skipped: captures quiesced for shutdown");
        return;
    }
    let due = backend.checkpoint_candidates_adaptive(cfg);
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
                //
                // Detached (engrams review, #835): the probe's up-to-6s
                // of confirmation sleeps ran INLINE in this serial pass
                // — with the quantum shrunk to `min_interval`, a few
                // dead guests could eat the whole tick and starve the
                // busy sandboxes' short epochs. Spawning is safe: the
                // probe only reads the socket and flips the (idempotent)
                // unreachable advert, and a healed guest is cleared by
                // its next successful capture above. Gated (review round
                // 2): at most ONE probe per sandbox at a time — a
                // still-failing sandbox is due EVERY tick (its
                // last-capture stamp never advances), and a
                // `min_interval` below the probe's ~6s lifetime would
                // otherwise stack overlapping probes against exactly the
                // guests least able to answer.
                if backend.try_begin_dead_probe(sandbox_id) {
                    let backend = Arc::clone(backend);
                    tokio::spawn(async move {
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
                        backend.end_dead_probe(sandbox_id);
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;

    /// ADR 0101 B: the adaptive controller's math — proportional to the
    /// observed dirty rate, clamped to [min, max], and falling back to
    /// the max backstop whenever there is no usable rate signal.
    #[test]
    fn next_epoch_scales_with_dirty_rate_and_clamps() {
        let min = Duration::from_secs(30);
        let max = Duration::from_secs(600);
        let target = 256 * 1024 * 1024u64;

        // Exactly on target: keep the same epoch.
        assert_eq!(
            next_epoch_after(Duration::from_secs(120), Some(target), min, max, target),
            Duration::from_secs(120),
        );
        // Half the target dirt → stretch the epoch 2×.
        assert_eq!(
            next_epoch_after(Duration::from_secs(120), Some(target / 2), min, max, target),
            Duration::from_secs(240),
        );
        // A dirt firehose (32× target in one epoch) → clamped to the floor.
        assert_eq!(
            next_epoch_after(
                Duration::from_secs(600),
                Some(target * 32),
                min,
                max,
                target
            ),
            min,
        );
        // Nearly idle → clamped to the max backstop.
        assert_eq!(
            next_epoch_after(Duration::from_secs(60), Some(1024), min, max, target),
            max,
        );
        // An inverted pair (operator lowered the ceiling below the MIN
        // default) must degrade to the ceiling, never panic the driver
        // task (engrams review, #835: f64::clamp panics on min > max).
        assert_eq!(
            next_epoch_after(
                Duration::from_secs(600),
                Some(target * 32),
                Duration::from_secs(30),
                Duration::from_secs(10),
                target
            ),
            Duration::from_secs(10),
        );
        assert_eq!(
            next_epoch_after(
                Duration::from_secs(60),
                None,
                Duration::from_secs(30),
                Duration::from_secs(10),
                target
            ),
            Duration::from_secs(10),
        );
        // No rate signal (Full capture / first epoch / zero dirty / zero
        // target) → the max backstop, never the floor.
        assert_eq!(
            next_epoch_after(Duration::from_secs(60), None, min, max, target),
            max
        );
        assert_eq!(
            next_epoch_after(Duration::from_secs(60), Some(0), min, max, target),
            max,
        );
        assert_eq!(
            next_epoch_after(Duration::ZERO, Some(target), min, max, target),
            max
        );
        assert_eq!(
            next_epoch_after(Duration::from_secs(60), Some(target), min, max, 0),
            max,
        );
    }

    fn record(id: SandboxId) -> ChainHeadRecord {
        ChainHeadRecord {
            sandbox_id: id,
            manifest_ref: ManifestRef::new(),
            session_id: None,
            updated_at: Utc::now(),
        }
    }

    /// The detached-tail interleaving from the adversarial review: a
    /// persist whose awaiting future was cancelled keeps running on the
    /// blocking pool while the capture lock is released; a NEWER
    /// capture's write-ahead invalidate then runs, and the tail's
    /// rename must NOT resurrect the old head afterwards. Simulated
    /// deterministically by capturing the epoch (what a persist does at
    /// initiation) and running the blocking transaction only AFTER the
    /// invalidate.
    #[tokio::test]
    async fn stale_persist_tail_cannot_resurrect_an_invalidated_record() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ChainHeadStore::new(tmp.path());
        let id = SandboxId::new();

        // Baseline: a current-epoch persist publishes.
        let first = record(id);
        store.persist(first.clone()).await.unwrap();
        assert_eq!(
            ChainHeadRecord::load(store.dir(), id)
                .await
                .unwrap()
                .manifest_ref,
            first.manifest_ref,
        );

        // The tail: initiated (epoch captured) before the next
        // capture's invalidate...
        let stale_epoch = store.epoch_for_test(id);
        let stale = record(id);
        // ...the next capture invalidates (bump + unlink)...
        store.invalidate(id).unwrap();
        assert!(ChainHeadRecord::load(store.dir(), id).await.is_none());
        // ...and the detached transaction finally runs: it must refuse.
        let st = store.state(id);
        ChainHeadStore::persist_at_epoch(store.dir(), &st, stale, stale_epoch).unwrap();
        assert!(
            ChainHeadRecord::load(store.dir(), id).await.is_none(),
            "a persist initiated before an invalidate must never publish after it",
        );
        // No stray temp files left behind either.
        let leftovers = std::fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(leftovers, 0, "superseded persist must clean its temp file");

        // The ratchet resumes: a persist initiated AFTER the invalidate
        // publishes normally.
        let next = record(id);
        store.persist(next.clone()).await.unwrap();
        assert_eq!(
            ChainHeadRecord::load(store.dir(), id)
                .await
                .unwrap()
                .manifest_ref,
            next.manifest_ref,
        );
    }
}
