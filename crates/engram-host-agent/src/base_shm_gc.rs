//! ADR 0045 D4: GC for the substrate's per-image base shm directory.
//!
//! Every substrate restore opens (creating if absent) a manifest-keyed
//! base file under `ENGRAM_FC_UFFD_BASE_DIR`. With the D4 canonical split,
//! steady state is one file per enabled image — but disabled/refreshed
//! images, pre-D4 session-keyed files, and reattach-fallback files would
//! otherwise accumulate on the tmpfs forever (the tmpfs size cap turns
//! that into loud restore failures eventually).
//!
//! Deletion safety: a base file is a CACHE — recreated + lazily
//! repopulated by the next restore that derives its path — EXCEPT while a
//! live VM maps it: the handler resolves `UFFDIO_CONTINUE` against the
//! page cache of the inode the VM's `MAP_PRIVATE` mapping is backed by,
//! so deleting + recreating underneath a live handler would split the
//! file identity and wedge faults. The live-ref signal is free: each
//! handler holds its `BaseShm` `File` OPEN for exactly the VM's lifetime,
//! so "some process holds this file open" (a `/proc/*/fd` scan) is a
//! precise keep-set with zero cross-process bookkeeping.
//!
//! Sweep rule: delete a regular file iff (a) its mtime is older than the
//! grace window (covers the spawn→open race of an in-flight restore and
//! keeps actively-populated files — `pwrite` refreshes mtime), AND (b) no
//! process holds it open. Conservative on any read error: keep.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default sweep cadence.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Files younger than this are always kept (in-flight restore grace +
/// "recently used" warmth).
pub const MTIME_GRACE: Duration = Duration::from_secs(30 * 60);

/// One sweep pass. Returns (kept, deleted) counts; errors are logged and
/// treated as "keep".
pub fn sweep(dir: &Path, grace: Duration) -> (usize, usize) {
    // The live-ref signal is /proc-based; without it (non-Linux dev
    // machines) deleting would be unsafe, so the sweep is a no-op.
    if !cfg!(target_os = "linux") {
        return (0, 0);
    }
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
        if !age_ok || open_paths.contains(&path) {
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
/// is off. The task is detached — it holds no state beyond the dir path
/// and dies with the process.
pub fn spawn(dir: Option<PathBuf>) -> Option<tokio::task::JoinHandle<()>> {
    let dir = dir?;
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let d = dir.clone();
            let (kept, deleted) = tokio::task::spawn_blocking(move || sweep(&d, MTIME_GRACE))
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

        let (kept, deleted) = sweep(dir.path(), Duration::from_secs(3600));
        assert_eq!(deleted, 1, "only the stale unopened file goes");
        assert_eq!(kept, 2);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(held_path.exists(), "open files must never be swept");
    }
}
