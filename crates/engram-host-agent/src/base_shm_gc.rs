//! ADR 0045 D4: GC for the substrate's per-image base shm directory.
//!
//! Every substrate restore opens (creating if absent) a manifest-keyed
//! base file under `ENGRAM_FC_UFFD_BASE_DIR`. With the D4 canonical split,
//! steady state is one file per enabled image — but disabled/refreshed
//! images, pre-D4 session-keyed files, and reattach-fallback files would
//! otherwise accumulate on the tmpfs forever (the tmpfs size cap turns
//! that into loud restore failures eventually).
//!
//! Deletion safety: a base file is recreated + lazily repopulated by the
//! next restore that derives its path, but deleting one is never free —
//! and in two cases it's a bug:
//!
//! - While a live VM maps it: the handler resolves `UFFDIO_CONTINUE`
//!   against the page cache of the inode the VM's `MAP_PRIVATE` mapping
//!   is backed by, so deleting + recreating underneath a live handler
//!   would split the file identity and wedge faults. The live-ref signal
//!   is free: each handler holds its `BaseShm` `File` OPEN for exactly
//!   the VM's lifetime, so "some process holds this file open" (a
//!   `/proc/*/fd` scan) is a precise keep-set with zero cross-process
//!   bookkeeping.
//! - While its image is ENABLED: `image_prefetch` pre-warms the file at
//!   readiness (ADR 0045 C1) and the scheduler places sessions on the
//!   strength of that residency. Sweeping it voids the C1 guarantee —
//!   the next session pays lazy population at NVMe latency under its
//!   own resume storm (see the ADR 0045 addendum, 2026-07-10: a host
//!   roll drained the old pod's VMs, the orphaned-but-enabled base
//!   file aged past grace, and the sweep deleted its accumulated
//!   residency). Open FDs don't cover this — VMs outlive the pod
//!   (ADR 0044 K2) but eventually drain, and the fully-populated
//!   file's mtime ages out. The [`ProtectedPaths`] registry closes it:
//!   the prefetch supervisor publishes the enabled images' base-file
//!   paths, and the sweep never deletes a member. Until the FIRST
//!   authoritative enabled-set arrives (heartbeat ack), the sweep
//!   deletes NOTHING — a fresh pod must not sweep on a stale view it
//!   doesn't have yet.
//!
//! Sweep rule: delete a regular file iff (a) the enabled-image keep-set
//! is KNOWN and doesn't contain it, (b) its mtime is older than the
//! grace window (covers the spawn→open race of an in-flight restore and
//! keeps actively-populated files — `pwrite` refreshes mtime), AND (c)
//! no process holds it open. Conservative on any read error: keep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

/// The enabled-image keep-set the sweep consults: the per-image base-file
/// paths of every currently-enabled image, published by the image-prefetch
/// supervisor on each heartbeat-ack reconcile. `None` (the initial state)
/// means "no authoritative enabled set seen yet" — the sweep treats that
/// as keep-everything, so a freshly-rolled pod can never delete an enabled
/// image's residency in the window before its first heartbeat ack.
#[derive(Default)]
pub struct ProtectedPaths {
    inner: RwLock<Option<HashSet<PathBuf>>>,
}

impl ProtectedPaths {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace the keep-set with the authoritative enabled-image view.
    /// An empty set is a valid value ("nothing is enabled — everything
    /// stale is sweepable") and distinct from the initial `None`.
    pub fn replace(&self, paths: HashSet<PathBuf>) {
        *self.inner.write() = Some(paths);
    }

    /// Snapshot for one sweep pass.
    fn snapshot(&self) -> Option<HashSet<PathBuf>> {
        self.inner.read().clone()
    }
}

/// Default sweep cadence.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Files younger than this are always kept (in-flight restore grace +
/// "recently used" warmth).
pub const MTIME_GRACE: Duration = Duration::from_secs(30 * 60);

/// One sweep pass. `protected` is the enabled-image keep-set (see
/// [`ProtectedPaths`]): `None` ⇒ the enabled set is unknown, delete
/// nothing. Returns (kept, deleted) counts; errors are logged and
/// treated as "keep".
pub fn sweep(dir: &Path, grace: Duration, protected: Option<&HashSet<PathBuf>>) -> (usize, usize) {
    // The live-ref signal is /proc-based; without it (non-Linux dev
    // machines) deleting would be unsafe, so the sweep is a no-op.
    if !cfg!(target_os = "linux") {
        return (0, 0);
    }
    let Some(protected) = protected else {
        tracing::debug!(
            dir = %dir.display(),
            "base-shm sweep: enabled-image set not yet known; deferring all deletes",
        );
        return (0, 0);
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "base-shm sweep: dir unreadable");
            return (0, 0);
        }
    };
    let open_paths = open_file_set();
    let now = std::time::SystemTime::now();
    let (mut kept, mut deleted) = (0usize, 0usize);
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let age_ok = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|age| age > grace)
            .unwrap_or(false);
        if protected.contains(&path) || !age_ok || open_paths.contains(&path) {
            kept += 1;
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(
                    file = %path.display(),
                    size = meta.len(),
                    "base-shm sweep: removed unreferenced base file"
                );
                deleted += 1;
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "base-shm sweep: remove failed");
                kept += 1;
            }
        }
    }
    (kept, deleted)
}

/// Every regular-file path currently open by any process on the host
/// (best-effort `/proc/*/fd` readlink scan; unreadable pids — other
/// users' processes, races with exits — are skipped, which is safe here
/// because our handlers run as the same user tree as the host-agent).
fn open_file_set() -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    let procs = match std::fs::read_dir("/proc") {
        Ok(p) => p,
        Err(_) => return set,
    };
    for p in procs.flatten() {
        let name = p.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fd_dir = p.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                set.insert(target);
            }
        }
    }
    set
}

/// Spawn the periodic sweeper. No-op (returns `None`) when the substrate
/// is off. The task is detached — it holds the dir path plus the shared
/// enabled-image keep-set and dies with the process.
pub fn spawn(
    dir: Option<PathBuf>,
    protected: Arc<ProtectedPaths>,
) -> Option<tokio::task::JoinHandle<()>> {
    let dir = dir?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let d = dir.clone();
            let keep = protected.snapshot();
            let (kept, deleted) =
                tokio::task::spawn_blocking(move || sweep(&d, MTIME_GRACE, keep.as_ref()))
                    .await
                    .unwrap_or((0, 0));
            if deleted > 0 {
                tracing::info!(kept, deleted, dir = %dir.display(), "base-shm sweep complete");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn old_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);
        // Backdate mtime past the grace window.
        let old = filetime::FileTime::from_unix_time(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 7200) as i64,
            0,
        );
        filetime::set_file_mtime(&p, old).unwrap();
        p
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "live-ref check is /proc-based")]
    fn sweep_deletes_old_unopened_keeps_fresh_and_open() {
        let dir = tempfile::tempdir().unwrap();

        // Fresh file: kept (grace).
        let fresh = dir.path().join("fresh.base");
        std::fs::write(&fresh, b"y").unwrap();

        // Old + unopened: deleted.
        let stale = old_file(dir.path(), "stale.base");

        // Old + HELD OPEN (the live-handler shape): kept.
        let held_path = old_file(dir.path(), "held.base");
        let _held = std::fs::File::open(&held_path).unwrap();

        let none_protected = HashSet::new();
        let (kept, deleted) = sweep(dir.path(), Duration::from_secs(3600), Some(&none_protected));
        assert_eq!(deleted, 1, "only the stale unopened file goes");
        assert_eq!(kept, 2);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(held_path.exists(), "open files must never be swept");
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "live-ref check is /proc-based")]
    fn sweep_keeps_enabled_images_base_files() {
        // The ADR 0045 addendum regression: an enabled image's base file
        // with no open FDs (its VMs drained after a pod roll) and an aged
        // mtime (fully populated long ago) must survive the sweep as long
        // as the image is enabled — and be sweepable once it isn't.
        let dir = tempfile::tempdir().unwrap();
        let enabled = old_file(dir.path(), "enabled-image.base");
        let stale = old_file(dir.path(), "disabled-image.base");

        let protected: HashSet<PathBuf> = [enabled.clone()].into_iter().collect();
        let (kept, deleted) = sweep(dir.path(), Duration::from_secs(3600), Some(&protected));
        assert_eq!((kept, deleted), (1, 1));
        assert!(enabled.exists(), "enabled image's base file must survive");
        assert!(!stale.exists());

        // Image disabled → out of the keep-set → next sweep reclaims it.
        let (kept, deleted) = sweep(dir.path(), Duration::from_secs(3600), Some(&HashSet::new()));
        assert_eq!((kept, deleted), (0, 1));
        assert!(!enabled.exists());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "live-ref check is /proc-based")]
    fn sweep_defers_all_deletes_until_enabled_set_is_known() {
        // A freshly-rolled pod sweeps before its first heartbeat ack: the
        // enabled set is unknown (`None`), so nothing may be deleted — the
        // node-surviving residency of a still-enabled image is
        // indistinguishable from garbage at that point.
        let dir = tempfile::tempdir().unwrap();
        let survivor = old_file(dir.path(), "roll-survivor.base");

        let (kept, deleted) = sweep(dir.path(), Duration::from_secs(3600), None);
        assert_eq!((kept, deleted), (0, 0));
        assert!(survivor.exists(), "unknown enabled set must defer deletes");
    }

    #[test]
    fn protected_paths_starts_unknown_and_replaces() {
        let p = ProtectedPaths::new();
        assert!(p.snapshot().is_none(), "initial state is 'unknown'");
        p.replace(HashSet::new());
        assert_eq!(p.snapshot(), Some(HashSet::new()), "empty is a real value");
        let set: HashSet<PathBuf> = [PathBuf::from("/shm/a.base")].into_iter().collect();
        p.replace(set.clone());
        assert_eq!(p.snapshot(), Some(set));
    }
}
