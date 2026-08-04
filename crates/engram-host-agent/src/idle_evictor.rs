//! Host-side disk-pressure utilities.
//!
//! HISTORICAL NOTE: this module was the host-side idle-eviction
//! *detection driver* (ADR 0011 follow-up #2 / ADR 0013) — it scanned
//! the local `HarnessHub` for over-TTL sandboxes and POSTed candidates
//! to the coordinator. ADR 0073 phase 4 retired that plane entirely:
//! the coordinator's PG-derived `idle_detector` is now the ONLY
//! detection plane (same semantics, sourced from the durable event log
//! instead of hub memory, which went amnesiac on every detach/restart).
//!
//! The ADR 0074 addendum (2026-07-13) removed the last vestige — the
//! module's TTL/pressure env helpers, which had no call sites and
//! misled readers into thinking the host still owned an idle policy.
//! What remains is the disk-pressure surface `materialize.rs` uses:
//! the free-space probe and its floor.

/// ADR 0014 issue #4: query the host's free disk via `statvfs(3)`.
/// Returns `Some(bytes)` on success, `None` if the FS lookup fails
/// (rare; the caller treats `None` as "no pressure, proceed" so we
/// fail open — never silently block evictions on a transient FS
/// error).
///
/// Cross-platform: statvfs exists on Linux + macOS, both of which
/// host-agent supports for dev/CI. The host-agent's `nix` crate is
/// configured with the `fs` feature for this entry point.
pub fn free_disk_bytes(path: &std::path::Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(path).ok()?;
    // `blocks_available` (free for non-root) × `fragment_size` (the
    // unit blocks_available counts in). Cast to u64 so the
    // multiplication is portable across 32-bit and 64-bit
    // libc::fsblkcnt_t / c_ulong widths.
    Some((stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64))
}

/// ADR 0014 issue #4: should the host's idle_evictor *push* idle
/// candidates this tick? Free disk under the floor → pause idle-
/// eviction so a runaway retry loop or slow snapshot uploader can't
/// fill the host disk (the `engrams-fc-xngk` failure mode). Returns
/// `(should_push, free_bytes)` so the caller can both gate AND
/// publish the gauge.
pub fn disk_pressure_check(work_dir: &std::path::Path, floor_bytes: u64) -> (bool, Option<u64>) {
    let free = free_disk_bytes(work_dir);
    let allow = match free {
        Some(b) => b >= floor_bytes,
        // Fail open: if statvfs erroring is somehow the new normal,
        // we'd rather over-evict than block all evictions silently.
        None => true,
    };
    (allow, free)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `free_disk_bytes` should return Some for any existing path
    /// on every supported host (Linux + macOS). The exact value is
    /// machine-dependent — just sanity-check it's non-zero on a
    /// freshly-created tempdir.
    #[test]
    fn free_disk_bytes_returns_value_for_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = free_disk_bytes(dir.path()).expect("statvfs on tempdir");
        assert!(bytes > 0, "free bytes should be non-zero on a real FS");
    }

    /// `free_disk_bytes(nonexistent)` returns None — feeds into
    /// disk_pressure_check's fail-open branch.
    #[test]
    fn free_disk_bytes_none_for_missing_path() {
        let p = std::path::PathBuf::from("/definitely/does/not/exist/engram-test");
        assert!(free_disk_bytes(&p).is_none());
    }

    /// disk_pressure_check fails open when statvfs errors (the
    /// missing-path case). Idle-evict keeps running rather than
    /// silently stalling all evictions on a transient FS error.
    #[test]
    fn disk_pressure_check_fails_open_on_statvfs_error() {
        let p = std::path::PathBuf::from("/definitely/does/not/exist/engram-test");
        let (allow, free) = disk_pressure_check(&p, u64::MAX);
        assert!(allow, "must fail open when free bytes unknown");
        assert!(free.is_none());
    }

    /// With floor_bytes = 0, every real path passes (free >= 0 always).
    #[test]
    fn disk_pressure_check_allows_when_floor_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let (allow, free) = disk_pressure_check(dir.path(), 0);
        assert!(allow);
        assert!(free.unwrap() > 0);
    }

    /// With floor_bytes = u64::MAX, no real path can pass — the
    /// gate blocks. Confirms the check uses `>=` against the floor
    /// in the right direction.
    #[test]
    fn disk_pressure_check_blocks_when_floor_exceeds_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let (allow, free) = disk_pressure_check(dir.path(), u64::MAX);
        assert!(!allow, "must block when free < floor");
        assert!(free.unwrap() > 0, "but free reading still surfaces");
    }

    // ── Tier 1: pressure-aware idle eviction ────────────────────────────
}
