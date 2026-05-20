//! Host-side idle-eviction detection driver (ADR 0011 follow-up #2,
//! landed via ADR 0013).
//!
//! The host's local `HarnessHub` is the authoritative source of
//! "this sandbox's adapter has been quiet for N seconds" — only the
//! host sees every harness event in real-time. In the pre-0013
//! single-replica coord world, the coord polled the hub directly
//! because the hub was in-proc. With a stateless coord, no single
//! pod's local hub is authoritative anymore (events fan in via
//! HTTP POSTs from multiple hosts to multiple coord pods).
//!
//! Resolution: the host runs the *driver* (this module), the coord
//! runs the *pipeline* (`engram_coordinator::idle_evictor::
//! evict_idle_session`). The driver scans the local hub on a tick,
//! finds candidates past TTL, POSTs them to
//! `/api/hosts/:id/idle-eviction-candidates`. The receiving coord
//! pod (any pod) runs the pipeline; the pipeline is idempotent so
//! multiple pods receiving the same batch (or the same candidate
//! batched across two ticks) doesn't cause double-eviction.

use std::time::Duration;

/// Soft idle TTL — a session whose adapter emitted `Idle` and stayed
/// quiet for this long is hot-suspended. Matches the coord-side
/// constant.
pub const DEFAULT_IDLE_TTL_SECS: u64 = 30;

/// Hard idle TTL — backstop for adapters that go silent without ever
/// emitting `Idle` (stuck in a tool call, infinite loop). Matches
/// the coord-side constant.
pub const DEFAULT_IDLE_HARD_TTL_SECS: u64 = 1800;

/// How often the host scans for over-TTL sandboxes.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Read `ENGRAM_IDLE_TTL_SECS` (soft TTL) — falls through to default.
pub fn idle_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_TTL_SECS))
}

/// Read `ENGRAM_IDLE_HARD_TTL_SECS` (hard TTL) — falls through to default.
pub fn idle_hard_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_HARD_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_HARD_TTL_SECS))
}

/// ADR 0014 issue #4 default disk-pressure floor. 20 GiB worst-case
/// is N=5 concurrent in-flight idle-evicts × ~4 GiB per FC memory
/// dump + headroom. Picked from the `engrams-fc-xngk` post-mortem:
/// the 99 GB disk filled in ~13 min at ~25 attempted evictions
/// × 4 GiB; 20 GiB free is the safety margin under which idle-evict
/// stops pushing new candidates.
pub const DEFAULT_DISK_FLOOR_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Read `ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES` — falls through to
/// [`DEFAULT_DISK_FLOOR_BYTES`].
pub fn disk_floor_bytes_from_env() -> u64 {
    std::env::var("ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DISK_FLOOR_BYTES)
}

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
}
