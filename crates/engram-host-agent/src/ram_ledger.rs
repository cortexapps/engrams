//! Host RAM ledger (issue #540).
//!
//! Before this module, the scheduler and the pressure-eviction gate
//! reasoned about host RAM from three unreconciled views: kernel
//! `MemAvailable` (observes everything, attributes nothing), the
//! `UtilizationProbe::sample` formula (silently assumed every resident
//! guest byte belongs to a reserving session), and the idle-evictor's
//! own private `/proc/meminfo` read. None of them could see where the
//! RAM actually went (base-shm tmpfs, a future parked-but-resident VM,
//! …), and the three views could — and under the parking ladder,
//! would — structurally disagree.
//!
//! This module is the fix: **one per-tick snapshot**
//! ([`RamLedgerSnapshot`]), every MiB of host RAM charged to exactly
//! one named bucket, and a single derivation
//! ([`RamLedgerSnapshot::allocatable_mib`]) that both the heartbeat and
//! the pressure gate consume. [`RamLedger`] is the long-lived half: it
//! tracks base-shm prewarm charges that are registered *before* the
//! multi-GiB write lands, so placement sees the charge within one
//! heartbeat tick of prewarm start instead of minutes later when the
//! write finishes.
//!
//! **The invariant this module makes structural:** a host-resident
//! sandbox's PSS is added back into `allocatable_mib` iff its session
//! holds a coordinator memory reservation. The host-side proxy for
//! "holds a reservation" is the per-sandbox `parked` flag
//! (`engram-sandbox-firecracker`'s `LiveSandbox::parked`, always
//! `false` until epic-parking-ladder lands). Parked residents are
//! charged at their full measured PSS — prod observed rss ≈ pss
//! (12.39 GB ≈ 12.39 GB) for the running dev-brain sandbox on
//! 2026-07-01, so no sharing discount is ever assumed; density math
//! that wants a sharing credit must first observe Σpss/Σrss < 1.0 on
//! the existing gauges.

use std::collections::HashMap;
use std::path::Path;

use engram_core::traits::sandbox::GuestMemoryStats;
use engram_core::types::manifest::ManifestRef;
use parking_lot::Mutex;

const MIB: u64 = 1024 * 1024;

/// One host RAM snapshot, sampled once per heartbeat tick. Every MiB of
/// host RAM is charged to exactly one bucket; [`Self::allocatable_mib`]
/// is the single derivation both the heartbeat and the pressure gate
/// consume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RamLedgerSnapshot {
    /// `false` on non-Linux (no `/proc`) or when `MemTotal` reads as 0
    /// (a `/proc/meminfo` parse failure) — every other field is 0 and
    /// placement/pressure fall back to their existing soft posture
    /// (unmeasured, not "host has zero RAM").
    pub measured: bool,
    /// `/proc/meminfo` `MemTotal`.
    pub mem_total_mib: u64,
    /// `/proc/meminfo` `MemAvailable` — the kernel's own reclaimable
    /// estimate. Already nets out populated tmpfs/shmem pages (base-shm
    /// included) and any RAM-resident-but-not-mapped-by-us memory,
    /// because shmem sits on the anon LRU and is excluded from this
    /// figure by the kernel's own accounting.
    pub mem_available_mib: u64,
    /// Σ smaps_rollup PSS over FC sandboxes NOT flagged `parked` — i.e.
    /// sandboxes whose session holds a coordinator memory reservation.
    /// The only bucket ever added back into `allocatable_mib`.
    pub running_vm_pss_mib: u64,
    /// Σ smaps_rollup PSS over FC sandboxes flagged `parked` — RAM-
    /// resident, reservation-free (epic-parking-ladder rungs 2-3).
    /// Always 0 until a backend ever parks a sandbox. Never added back
    /// into `allocatable_mib` — see the module invariant above. This is
    /// the seam the ladder/queue-scanner later use to count
    /// *reclaimable-under-pressure* capacity; deliberately NOT folded
    /// into placement math by this issue.
    pub parked_paused_pss_mib: u64,
    /// Σ allocated blocks (`st_blocks * 512`, NOT `st_size` — base-shm
    /// files are sparse and grow-only) over regular files in the base
    /// shm dir (`ENGRAM_FC_UFFD_BASE_DIR`). What's actually resident on
    /// the tmpfs right now.
    pub base_shm_mib: u64,
    /// Registered prewarm charges not yet reflected in
    /// `base_shm_mib` — i.e. bytes `image_prefetch` has promised to
    /// write but the pwrite loop hasn't finished (or the file hasn't
    /// been re-scanned) yet. Subtracted from `allocatable_mib` so
    /// placement sees the charge within one heartbeat tick of prewarm
    /// start rather than minutes later when the write completes.
    pub base_shm_pending_mib: u64,
    /// `statfs` total capacity (MiB) of the base-shm tmpfs — the fixed
    /// `uffdBaseTmpfsSize` cap. Surfaces the ceiling before it's hit
    /// (the 2026-06-28 `pwrite ... No space left on device` incident
    /// class), alongside `base_shm_mib` for a used/total ratio.
    pub base_shm_tmpfs_total_mib: u64,
    /// NVMe-resident retained memfiles (epic-parking-ladder rung 3).
    /// DISK-side, gauge-only here — chunk-cache-disk-budget (#528) owns
    /// charging it against a disk budget. Always 0 until that ladder
    /// rung exists; never folded into `allocatable_mib` (RAM-only).
    pub parked_local_memfile_mib: u64,
}

impl RamLedgerSnapshot {
    /// ADR 0046, amended by issue #540: `allocatable = MemAvailable +
    /// Σ PSS of reservation-backed (running, non-parked) VMs − pending
    /// base-shm charges`. `MemAvailable` already nets out populated
    /// tmpfs (shmem is not reclaimable) and any parked residents (they
    /// sit on the same LRU); we add back ONLY the VMs whose sessions
    /// hold a coordinator memory reservation, so placement can subtract
    /// each session's full budget without double-counting what those
    /// VMs already occupy. Parked PSS is NEVER added back — that's the
    /// fix for the double-count the parking ladder would otherwise
    /// introduce (a parked VM's bytes are already excluded from
    /// `MemAvailable`; adding them back a second time here would make
    /// placement think the same RAM is both occupied and free).
    pub fn allocatable_mib(&self) -> u64 {
        self.mem_available_mib
            .saturating_add(self.running_vm_pss_mib)
            .saturating_sub(self.base_shm_pending_mib)
    }

    /// The pressure gate's number — same snapshot, one derivation, so
    /// the heartbeat's `allocatable_mib` and the evictor's `free_pct`
    /// can never disagree. `None` when unmeasured (mirrors
    /// `mem_pressure_from`'s `total_mib == 0` fail-open case).
    pub fn free_pct(&self) -> Option<f32> {
        if !self.measured || self.mem_total_mib == 0 {
            return None;
        }
        Some((self.mem_available_mib as f32 / self.mem_total_mib as f32) * 100.0)
    }
}

/// Long-lived half of the ledger: the pending-charge registry for
/// in-flight base-shm prewarms. One instance lives for the host-agent
/// process lifetime, shared between `image_prefetch` (writer) and the
/// heartbeat tick (reader, via [`RamLedger::sample`]).
#[derive(Default)]
pub struct RamLedger {
    /// Manifest-ref-keyed because a prewarm is per memory manifest —
    /// the same key `image_prefetch` already uses to address the
    /// base-shm file (`uffd_base_path_in`). Bytes, not MiB, so the
    /// pending charge is exact until the final MiB rounding in
    /// `sample`.
    pending: Mutex<HashMap<ManifestRef, u64>>,
}

impl RamLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an expected base-shm write BEFORE calling
    /// `prewarm_base_shm`, so the very next heartbeat tick charges the
    /// bytes against `allocatable_mib` — the prewarm-charging fix.
    /// `bytes` is the manifest's non-hole byte total (Σ chunk lengths),
    /// not `total_bytes` (which includes elided/zero ranges the
    /// prewarm never writes).
    pub fn register_pending_base_shm(&self, key: ManifestRef, bytes: u64) {
        self.pending.lock().insert(key, bytes);
    }

    /// Settle (remove) a pending charge — call in BOTH the success and
    /// failure arms after `prewarm_base_shm` returns. On success the
    /// bytes are now on the tmpfs and the next `sample`'s `st_blocks`
    /// scan measures them into `base_shm_mib`; on failure the lazy
    /// per-fault backstop takes over and no bytes were actually
    /// reserved, so there is nothing left to charge here either way.
    pub fn settle_pending(&self, key: &ManifestRef) {
        self.pending.lock().remove(key);
    }

    fn pending_total_bytes(&self) -> u64 {
        self.pending.lock().values().sum()
    }

    /// Build one snapshot: the meminfo read, the guest-PSS split
    /// (already bucketed by the backend's `parked` flag), the base-shm
    /// `st_blocks` dir walk + tmpfs statfs, and this ledger's own
    /// pending-charge total. Every read fails soft to 0 — telemetry
    /// must never gate or crash the heartbeat loop.
    pub fn sample(&self, base_dir: Option<&Path>, guest: &GuestMemoryStats) -> RamLedgerSnapshot {
        let (mem_total_mib, mem_used_mib) = crate::util::mem_mib();
        let mem_available_mib = mem_total_mib.saturating_sub(mem_used_mib);
        // `mem_mib()` returns (0, 0) both on non-Linux and on a genuine
        // parse failure; a real host always reports a nonzero MemTotal,
        // so "total == 0" is the one signal available to distinguish
        // "unmeasured" from "measured and legitimately idle" — the same
        // heuristic `mem_pressure_from`'s `total_mib == 0` check uses.
        let measured = mem_total_mib > 0;
        let (base_shm_mib, base_shm_tmpfs_total_mib) = match base_dir {
            Some(dir) => tmpfs_stat_mib(dir),
            None => (0, 0),
        };
        RamLedgerSnapshot {
            measured,
            mem_total_mib,
            mem_available_mib,
            running_vm_pss_mib: guest.pss_bytes / MIB,
            parked_paused_pss_mib: guest.parked_pss_bytes / MIB,
            base_shm_mib,
            base_shm_pending_mib: self.pending_total_bytes() / MIB,
            base_shm_tmpfs_total_mib,
            // Rung 3 (parked-local-memfile) is disk-side and doesn't
            // exist yet; chunk-cache-disk-budget (#528) owns it.
            parked_local_memfile_mib: 0,
        }
    }
}

/// `(used_mib, total_mib)` for the base-shm dir: `used` is Σ allocated
/// blocks (`st_blocks * 512`) over its regular files — the ACTUAL
/// tmpfs bytes resident, not the sparse `st_size` (a base-shm file is
/// grow-only and holes stay unwritten). `total` is `statfs`'s
/// `f_blocks * f_frsize` — the tmpfs's fixed size cap. `(0, 0)` on any
/// read error (missing dir, permission, non-Linux): fail soft.
#[cfg(target_os = "linux")]
fn tmpfs_stat_mib(dir: &Path) -> (u64, u64) {
    let used_mib = base_shm_used_mib(dir);
    let total_mib = match nix::sys::statvfs::statvfs(dir) {
        Ok(stat) => {
            let frag: u64 = stat.fragment_size();
            stat.blocks().saturating_mul(frag) / MIB
        }
        Err(_) => 0,
    };
    (used_mib, total_mib)
}

#[cfg(not(target_os = "linux"))]
fn tmpfs_stat_mib(_dir: &Path) -> (u64, u64) {
    (0, 0)
}

#[cfg(target_os = "linux")]
fn base_shm_used_mib(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total_blocks: u64 = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // `st_blocks` is always in 512-byte units regardless of the
        // filesystem's actual block size (POSIX `stat(2)`).
        total_blocks = total_blocks.saturating_add(meta.blocks());
    }
    total_blocks.saturating_mul(512) / MIB
}

/// Free MiB on the base-shm tmpfs, for the prewarm headroom pre-check
/// (`image_prefetch`'s "skip the write if it won't fit" arm). Returns
/// `None` if the dir isn't statfs-able (fail-soft: the caller treats
/// "unknown" the same as "don't skip", preserving today's
/// warn-and-continue-then-lazy-backstop behavior for anything this
/// check can't see).
#[cfg(target_os = "linux")]
pub fn tmpfs_free_mib(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    let frag: u64 = stat.fragment_size();
    Some(stat.blocks_available().saturating_mul(frag) / MIB)
}

#[cfg(not(target_os = "linux"))]
pub fn tmpfs_free_mib(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        mem_total: u64,
        mem_avail: u64,
        running: u64,
        parked: u64,
        pending: u64,
    ) -> RamLedgerSnapshot {
        RamLedgerSnapshot {
            measured: true,
            mem_total_mib: mem_total,
            mem_available_mib: mem_avail,
            running_vm_pss_mib: running,
            parked_paused_pss_mib: parked,
            base_shm_mib: 0,
            base_shm_pending_mib: pending,
            base_shm_tmpfs_total_mib: 0,
            parked_local_memfile_mib: 0,
        }
    }

    #[test]
    fn parked_pss_is_never_added_back() {
        // Same host, same MemAvailable — the only difference is whether
        // a 12 GiB VM is flagged parked or omitted entirely. Parked RAM
        // must never be double-counted as placeable: the two must
        // report the SAME allocatable_mib.
        let with_parked = snapshot(64_000, 40_000, 10_000, 12_288, 0);
        let without_parked_vm = snapshot(64_000, 40_000, 10_000, 0, 0);
        assert_eq!(
            with_parked.allocatable_mib(),
            without_parked_vm.allocatable_mib(),
            "a parked VM's PSS must not change allocatable_mib at all"
        );
    }

    #[test]
    fn running_pss_is_added_back() {
        let s = snapshot(64_000, 40_000, 10_000, 0, 0);
        assert_eq!(s.allocatable_mib(), 50_000);
    }

    #[test]
    fn pending_base_shm_charge_lowers_allocatable() {
        let s = snapshot(64_000, 40_000, 0, 0, 19_000);
        assert_eq!(
            s.allocatable_mib(),
            21_000,
            "a registered-but-unwritten prewarm charge must be subtracted immediately"
        );
    }

    #[test]
    fn unmeasured_snapshot_reports_zero_free_pct() {
        let s = RamLedgerSnapshot::default();
        assert!(!s.measured);
        assert_eq!(s.free_pct(), None);
        assert_eq!(s.allocatable_mib(), 0);
    }

    #[test]
    fn free_pct_matches_mem_available_ratio() {
        let s = snapshot(100, 25, 0, 0, 0);
        assert_eq!(s.free_pct(), Some(25.0));
    }

    #[test]
    fn ledger_pending_charge_registers_and_settles() {
        // Goes through `RamLedger::sample` (the real `/proc/meminfo`
        // read), so `mem_available_mib` is whatever THIS machine
        // reports — 0 on non-Linux. The pending-charge bookkeeping
        // itself (register → visible in `base_shm_pending_mib` → gone
        // after settle) is what's under test; `allocatable_mib`'s
        // response to a pending charge is separately proven,
        // synthetic-snapshot-only, in
        // `pending_base_shm_charge_lowers_allocatable`.
        let ledger = RamLedger::new();
        let key = ManifestRef::new();
        let empty = GuestMemoryStats::default();

        let before = ledger.sample(None, &empty);
        assert_eq!(before.base_shm_pending_mib, 0);

        ledger.register_pending_base_shm(key, 19 * MIB * 1024); // ~19 GiB
        let during = ledger.sample(None, &empty);
        assert_eq!(during.base_shm_pending_mib, 19 * 1024);
        assert!(during.allocatable_mib() <= before.allocatable_mib());

        ledger.settle_pending(&key);
        let after = ledger.sample(None, &empty);
        assert_eq!(after.base_shm_pending_mib, 0);
    }

    #[test]
    fn ledger_settle_of_unknown_key_is_a_noop() {
        let ledger = RamLedger::new();
        // Settling a charge that was never registered (e.g. a
        // duplicate settle, or a failure arm that races a success arm)
        // must not panic and must leave the pending total unchanged.
        ledger.settle_pending(&ManifestRef::new());
        assert_eq!(ledger.pending_total_bytes(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn base_shm_used_mib_counts_allocated_blocks_not_sparse_len() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("base.mem");
        let mut f = std::fs::File::create(&path).expect("create");
        // A sparse 64 MiB file with only 4 KiB actually written: the
        // logical size (`st_size`) is 64 MiB, but the ALLOCATED size
        // (`st_blocks * 512`) is a handful of KiB. If this measured
        // `st_size` instead, a single 64 MiB hole-punched manifest
        // range would report as if it were fully resident.
        f.seek(SeekFrom::Start(64 * 1024 * 1024 - 4096)).unwrap();
        f.write_all(&[0xAB; 4096]).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let logical_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            logical_len,
            64 * 1024 * 1024,
            "sparse file is logically 64 MiB"
        );

        let used_mib = base_shm_used_mib(dir.path());
        assert!(
            used_mib < 63,
            "allocated blocks ({used_mib} MiB) must reflect the ~4 KiB actually \
             written, not the 64 MiB logical length"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tmpfs_stat_mib_on_missing_dir_fails_soft() {
        let (used, total) = tmpfs_stat_mib(Path::new("/nonexistent/engram/base-shm-probe"));
        assert_eq!((used, total), (0, 0));
    }
}
