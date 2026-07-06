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
/// quiet for this long is hot-suspended.
///
/// ADR 0039 follow-up #20: bumped 30s → 300s (5 min). The 30s default
/// was too aggressive for an interactive agent session: the harness
/// emits `HarnessEvent::Idle` the moment it finishes a turn and has no
/// queued prompt, so an ordinary think-pause while the user reads the
/// output and composes the next message crosses 30s routinely (prod
/// observed a just-resumed session re-nominated 32s later). Each such
/// eviction pays a full snapshot+destroy and the next message a cold
/// resume — churn with no density benefit on a session a human is
/// actively driving. 5 min keeps the warm sandbox alive across normal
/// conversational gaps while still reclaiming genuinely-abandoned
/// sessions; the hard TTL ([`DEFAULT_IDLE_HARD_TTL_SECS`], 30 min)
/// still backstops adapters that never emit `Idle`. Operators tune via
/// `ENGRAM_IDLE_TTL_SECS` for denser-but-colder fleets.
pub const DEFAULT_IDLE_TTL_SECS: u64 = 300;

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

/// Tier 1 (pressure-aware idle eviction): master switch, read from
/// `ENGRAM_IDLE_EVICT_PRESSURE_AWARE`. **Default OFF** — when unset (or
/// not `1`/`true`) the host nominates every soft-idle candidate exactly
/// as it always has (TTL-only), so this ships dark and flips per-host via
/// env with an instant rollback.
///
/// When ON, a *soft*-idle sandbox is only nominated for eviction while the
/// host is under real memory pressure (see [`mem_pressure_from`]); *hard*-
/// idle sandboxes and the coord's own hard-TTL backstop are unaffected.
/// The rationale: eviction snapshots + destroys a warm VM to reclaim
/// **RAM**, and on a host with abundant free memory that just trades an
/// instant warm resume for a slow cold one with no density benefit — the
/// exact churn ADR 0039 follow-up #20 already softened via the 5-min TTL.
/// This makes the reclaim demand-driven instead of purely time-driven.
pub fn pressure_aware_from_env() -> bool {
    std::env::var("ENGRAM_IDLE_EVICT_PRESSURE_AWARE")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Default free-RAM floor (percent of `MemTotal`) below which soft-idle
/// sandboxes become eligible for reclamation under pressure-aware mode.
/// 15 % leaves generous headroom above OOM while still reclaiming before
/// the host genuinely runs out — the 10 s evict tick then has room to
/// shed the least-recently-active sessions.
pub const DEFAULT_MEM_FLOOR_PCT: u8 = 15;

/// Read `ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT` — falls through to
/// [`DEFAULT_MEM_FLOOR_PCT`]. Only consulted when
/// [`pressure_aware_from_env`] is on.
pub fn mem_floor_pct_from_env() -> u8 {
    std::env::var("ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(DEFAULT_MEM_FLOOR_PCT)
}

/// Is the host under memory pressure? Eviction frees **RAM** (the disk
/// floor above is an orthogonal *brake*, not this signal), so soft-idle
/// reclamation should only fire when free RAM has dropped below the floor.
/// Returns `(under_pressure, free_pct)`.
///
/// **Fails OPEN toward eviction**: if `total_mib` reads as 0 (non-Linux, an
/// unmeasured `RamLedgerSnapshot`, or a `/proc/meminfo` parse failure) we
/// report `(true, None)` so a read error degrades pressure-aware mode back
/// to today's TTL-only behavior rather than silently pinning soft-idle
/// sessions resident forever — mirroring [`disk_pressure_check`]'s
/// fail-open stance.
///
/// Issue #540: the caller feeds this the heartbeat tick's
/// `RamLedgerSnapshot` (via a `watch` channel) instead of this module
/// taking its own private `/proc/meminfo` sample — one source of truth for
/// both the heartbeat's `allocatable_mib` and this gate's `free_pct`. Pure
/// core, unit-testable without a live `/proc/meminfo`.
pub(crate) fn mem_pressure_from(
    total_mib: u64,
    used_mib: u64,
    floor_pct: u8,
) -> (bool, Option<f32>) {
    if total_mib == 0 {
        return (true, None);
    }
    let free_mib = total_mib.saturating_sub(used_mib);
    let free_pct = (free_mib as f32 / total_mib as f32) * 100.0;
    (free_pct < f32::from(floor_pct), Some(free_pct))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0039 follow-up #20: the soft idle TTL default is no longer
    /// the aggressive 30s that evicted interactive sessions during
    /// normal think-pauses. Guards against an accidental revert at
    /// compile time. The floor of "at least a couple of minutes" is
    /// what matters, not the exact value — and it must stay strictly
    /// below the hard TTL (the never-emits-Idle backstop) so the soft
    /// path still fires first for a genuinely abandoned session.
    const _SOFT_TTL_NOT_AGGRESSIVE: () = {
        assert!(DEFAULT_IDLE_TTL_SECS >= 120);
        assert!(DEFAULT_IDLE_TTL_SECS < DEFAULT_IDLE_HARD_TTL_SECS);
    };

    /// With no env override, `idle_ttl_from_env` returns the (bumped)
    /// default. Asserting the no-override branch keeps the test
    /// race-free — it never mutates the process-global env.
    #[test]
    fn idle_ttl_from_env_falls_through_to_default_without_override() {
        // The CI/dev environment does not set ENGRAM_IDLE_TTL_SECS; if a
        // local shell does, skip rather than assert a wrong value.
        if std::env::var("ENGRAM_IDLE_TTL_SECS").is_ok() {
            return;
        }
        assert_eq!(
            idle_ttl_from_env(),
            Duration::from_secs(DEFAULT_IDLE_TTL_SECS),
        );
    }

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

    /// Unreadable memory (`MemTotal == 0`: non-Linux or a parse failure)
    /// fails OPEN toward eviction — `(under_pressure = true, None)` — so a
    /// read error degrades to today's TTL-only behavior instead of pinning
    /// soft-idle sessions resident forever. Mirrors the disk fail-open.
    #[test]
    fn mem_pressure_from_fails_open_when_total_unknown() {
        let (under, pct) = mem_pressure_from(0, 0, 15);
        assert!(under, "unreadable RAM must fail open toward eviction");
        assert!(pct.is_none());
    }

    /// Abundant free RAM (well above the floor) → no pressure, so soft-idle
    /// sandboxes stay resident. 64 GiB host, 26 GiB used ≈ 59 % free ≫ 15 %.
    #[test]
    fn mem_pressure_from_no_pressure_when_free_above_floor() {
        let (under, pct) = mem_pressure_from(64_304, 26_456, 15);
        assert!(!under, "59% free is not pressure at a 15% floor");
        let p = pct.unwrap();
        assert!((55.0..65.0).contains(&p), "free_pct ~59, got {p}");
    }

    /// Scarce free RAM (below the floor) → pressure, so soft-idle sandboxes
    /// become reclaim candidates. 64 GiB host, 60 GiB used ≈ 6 % free < 15 %.
    #[test]
    fn mem_pressure_from_pressure_when_free_below_floor() {
        let (under, pct) = mem_pressure_from(64_304, 60_000, 15);
        assert!(under, "6% free is pressure at a 15% floor");
        assert!(pct.unwrap() < 15.0);
    }

    /// A floor of 0 never reports pressure (free_pct is always ≥ 0) — a
    /// clean off-switch equivalent to "reclaim only at the hard TTL".
    #[test]
    fn mem_pressure_from_floor_zero_never_pressures() {
        let (under, _) = mem_pressure_from(64_304, 64_000, 0);
        assert!(!under, "floor 0 must never pressure");
    }

    /// Without an env override, the master switch defaults OFF (historical
    /// TTL-only behavior) and the floor defaults to
    /// [`DEFAULT_MEM_FLOOR_PCT`]. Skips if the shell sets the vars, keeping
    /// the test race-free (never mutates process-global env).
    #[test]
    fn pressure_env_defaults_off_and_floor_default() {
        if std::env::var("ENGRAM_IDLE_EVICT_PRESSURE_AWARE").is_err() {
            assert!(!pressure_aware_from_env(), "must default OFF");
        }
        if std::env::var("ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT").is_err() {
            assert_eq!(mem_floor_pct_from_env(), DEFAULT_MEM_FLOOR_PCT);
        }
    }
}
