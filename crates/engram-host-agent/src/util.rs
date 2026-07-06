//! Observed host-utilization probe — disk, memory, and CPU sampled
//! once per heartbeat and shipped to the coord for the operator
//! fleet view (`HostUtilization`).
//!
//! This is deliberately *observed* utilization, not the scheduler's
//! reservation model (`HostCapacityReport.used_mib`): the fleet view
//! answers "is this host about to fall over?", which is a question
//! about real disk/RAM/CPU pressure, not committed guest RAM. Disk
//! is the one that bites in practice — the chunk cache, jails, and FC
//! memory dumps all land on the work_dir mount, and a full disk
//! bricks the host with no warning unless we surface it.
//!
//! Every read fails soft to 0 (telemetry must never gate or crash the
//! heartbeat loop — see `[telemetry_must_not_gate_workload]`). Disk
//! comes from `statvfs(2)` (Linux + macOS); memory and CPU come from
//! `/proc` and read as 0 on non-Linux, where the fleet view simply
//! renders an empty bar.

use std::path::Path;

use engram_core::types::host::HostUtilization;

const MIB: u64 = 1024 * 1024;

/// Env var: the kubelet's ephemeral-storage hard-eviction threshold, as
/// a percent of the filesystem (0-100) — the line
/// [`metrics::HOST_DISK_HEADROOM_TO_KUBELET_BYTES`] measures distance
/// to. Default 10 matches GKE's documented `nodefs.available < 10%`
/// default and is consistent with the 611v prod incident's arithmetic
/// (31,259,425,233 B free at eviction ≈ 10.5% of a 298.1 GB disk).
///
/// TODO(ADR 0067): this is a placeholder pending confirmation against
/// the actual nodepool/kubelet config in the engrams-internal deploy
/// repo — GKE lets an operator override the default via
/// `--eviction-hard`/`--system-reserved`, and this default has NOT been
/// cross-checked against that repo (out of scope for this OSS repo). Do
/// not treat `10` as prod-verified; confirm and adjust
/// `ENGRAM_KUBELET_EVICT_PCT` in the deploy chart's values, not here.
pub const KUBELET_EVICT_PCT_ENV_VAR: &str = "ENGRAM_KUBELET_EVICT_PCT";

/// See [`KUBELET_EVICT_PCT_ENV_VAR`]'s doc comment for the
/// needs-prod-verification caveat.
pub const DEFAULT_KUBELET_EVICT_PCT: f64 = 10.0;

/// Resolve [`DEFAULT_KUBELET_EVICT_PCT`] from env, fail-soft (out of
/// range or unparseable ⇒ default). Read once at
/// [`UtilizationProbe::new`] — the value is operator/deploy-time
/// config, not something that changes tick to tick.
fn resolve_kubelet_evict_frac() -> f64 {
    match std::env::var(KUBELET_EVICT_PCT_ENV_VAR) {
        Ok(raw) => match raw.parse::<f64>() {
            Ok(pct) if (0.0..=100.0).contains(&pct) => pct / 100.0,
            other => {
                tracing::warn!(
                    env = KUBELET_EVICT_PCT_ENV_VAR,
                    value = raw,
                    parsed = ?other,
                    "kubelet-evict-pct env var out of range [0,100] / unparseable; using default",
                );
                DEFAULT_KUBELET_EVICT_PCT / 100.0
            }
        },
        Err(_) => DEFAULT_KUBELET_EVICT_PCT / 100.0,
    }
}

/// Holds the cross-tick state the CPU calculation needs (a percentage
/// is a delta between two `/proc/stat` reads). Construct once, then
/// call [`UtilizationProbe::sample`] each heartbeat.
pub struct UtilizationProbe {
    prev_cpu: Option<CpuTimes>,
    /// ADR 0067: resolved once at construction (see
    /// [`resolve_kubelet_evict_frac`]).
    kubelet_evict_frac: f64,
}

impl Default for UtilizationProbe {
    fn default() -> Self {
        Self {
            prev_cpu: None,
            kubelet_evict_frac: resolve_kubelet_evict_frac(),
        }
    }
}

impl UtilizationProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample disk (for `work_dir`), memory, and CPU. The first call
    /// returns `cpu_pct = 0` because there's no prior sample to diff
    /// against; subsequent calls report utilization over the interval
    /// since the previous call.
    pub fn sample(&mut self, work_dir: &Path, guest_pss_mib: u64) -> HostUtilization {
        let (disk_total_mib, disk_used_mib) = self.sample_disk(work_dir);
        let (mem_total_mib, mem_used_mib) = mem_mib();
        let cpu_pct = self.cpu_pct();
        // ADR 0046: allocatable = MemAvailable + Σ guest-resident (PSS).
        // MemAvailable (= mem_total − mem_used) already nets out the daemon, OS,
        // chunk cache, and the mlock'd base-memfile residency; adding back the
        // running VMs' resident memory lets placement subtract each session's
        // FULL budget without double-counting what the VMs already occupy.
        let allocatable_mib = mem_total_mib
            .saturating_sub(mem_used_mib)
            .saturating_add(guest_pss_mib);
        HostUtilization {
            disk_total_mib,
            disk_used_mib,
            mem_total_mib,
            mem_used_mib,
            allocatable_mib,
            cpu_pct,
        }
    }

    /// `(total_mib, used_mib)` for the filesystem backing `path` (fails
    /// soft to `(0, 0)`) — plus, as a side effect, emits
    /// [`crate::metrics::HOST_DISK_HEADROOM_TO_KUBELET_BYTES`] (ADR
    /// 0067): `fs_free - fs_total * kubelet_evict_frac`. A probe failure
    /// skips the gauge entirely rather than emit a misleading 0
    /// (telemetry must never gate or crash the heartbeat loop, but it
    /// also shouldn't lie).
    fn sample_disk(&self, path: &Path) -> (u64, u64) {
        let Some((total, free)) = disk_stat_bytes(path) else {
            return (0, 0);
        };
        let used = total.saturating_sub(free);

        let evict_floor_bytes = (total as f64 * self.kubelet_evict_frac) as u64;
        let headroom = free as i64 - evict_floor_bytes as i64;
        ::metrics::gauge!(crate::metrics::HOST_DISK_HEADROOM_TO_KUBELET_BYTES).set(headroom as f64);

        (total / MIB, used / MIB)
    }

    fn cpu_pct(&mut self) -> f32 {
        let cur = match read_cpu_times() {
            Some(c) => c,
            None => return 0.0,
        };
        let pct = match self.prev_cpu {
            Some(prev) => {
                let d_total = cur.total.saturating_sub(prev.total);
                let d_idle = cur.idle.saturating_sub(prev.idle);
                if d_total == 0 {
                    0.0
                } else {
                    let busy = d_total.saturating_sub(d_idle) as f64;
                    ((busy / d_total as f64) * 100.0).clamp(0.0, 100.0) as f32
                }
            }
            // First tick: no baseline yet.
            None => 0.0,
        };
        self.prev_cpu = Some(cur);
        pct
    }
}

/// `(total_bytes, free_bytes)` for the filesystem backing `path`, via
/// `statvfs(2)`. `free` is availability to an unprivileged writer
/// (`f_bavail`), matching what `df` reports. `None` if the probe fails.
/// The one `statvfs` call site — [`UtilizationProbe::sample_disk`]
/// derives both the fleet-view `(total_mib, used_mib)` pair and the
/// ADR-0067 kubelet-headroom gauge from it.
fn disk_stat_bytes(path: &Path) -> Option<(u64, u64)> {
    let stat = nix::sys::statvfs::statvfs(path).ok()?;
    let frag = stat.fragment_size() as u64;
    let total = (stat.blocks() as u64).saturating_mul(frag);
    let free = (stat.blocks_available() as u64).saturating_mul(frag);
    Some((total, free))
}

/// `(total_mib, used_mib)` of physical RAM, from `/proc/meminfo`
/// (`MemTotal`, `MemAvailable`, both in kB). `used = total − available`
/// — the kernel's own estimate of memory the workload can't reclaim.
/// `(0, 0)` on non-Linux or any parse failure.
#[cfg(target_os = "linux")]
pub(crate) fn mem_mib() -> (u64, u64) {
    let text = match std::fs::read_to_string("/proc/meminfo") {
        Ok(t) => t,
        Err(_) => return (0, 0),
    };
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_meminfo_kb(rest);
        }
    }
    let total_mib = total_kb / 1024;
    let used_mib = total_kb.saturating_sub(avail_kb) / 1024;
    (total_mib, used_mib)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn mem_mib() -> (u64, u64) {
    (0, 0)
}

/// Parse the numeric kB value out of a `/proc/meminfo` value field
/// like `"  32896180 kB"`. Returns 0 on any malformed line.
#[cfg(target_os = "linux")]
fn parse_meminfo_kb(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Aggregate jiffies from the `cpu` line of `/proc/stat`.
#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

/// Read the aggregate-cpu line from `/proc/stat`. `idle` folds in
/// `iowait` (both are time the CPU wasn't doing work). `None` on
/// non-Linux or parse failure.
#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<CpuTimes> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    // First line: "cpu  user nice system idle iowait irq softirq steal guest guest_nice"
    let line = text.lines().next()?;
    let fields = line.strip_prefix("cpu")?.trim_start();
    let vals: Vec<u64> = fields
        .split_whitespace()
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    // Need at least up through idle (index 3) + iowait (index 4).
    if vals.len() < 5 {
        return None;
    }
    let idle = vals[3].saturating_add(vals[4]); // idle + iowait
    let total: u64 = vals.iter().copied().sum();
    Some(CpuTimes { total, idle })
}

#[cfg(not(target_os = "linux"))]
fn read_cpu_times() -> Option<CpuTimes> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_stat_bytes_reports_nonzero_for_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (total, free) = disk_stat_bytes(dir.path()).expect("a real FS should be probeable");
        assert!(total > 0, "a real filesystem should report a total size");
        assert!(free <= total, "free must not exceed total");
    }

    #[test]
    fn disk_stat_bytes_none_on_bad_path() {
        assert_eq!(
            disk_stat_bytes(Path::new("/nonexistent/engram/probe/path")),
            None,
        );
    }

    // ---- ADR 0067: kubelet-headroom gauge ----

    // Tests poke a process-global env var; serialize (mirrors the
    // ENV_LOCK pattern in engram-chunk-store's cache.rs tests).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn kubelet_evict_frac_defaults_when_unset() {
        let _g = env_guard();
        std::env::remove_var(KUBELET_EVICT_PCT_ENV_VAR);
        assert_eq!(
            resolve_kubelet_evict_frac(),
            DEFAULT_KUBELET_EVICT_PCT / 100.0,
        );
    }

    #[test]
    fn kubelet_evict_frac_env_overrides_default() {
        let _g = env_guard();
        std::env::set_var(KUBELET_EVICT_PCT_ENV_VAR, "15");
        let frac = resolve_kubelet_evict_frac();
        std::env::remove_var(KUBELET_EVICT_PCT_ENV_VAR);
        assert!((frac - 0.15).abs() < 1e-9);
    }

    #[test]
    fn kubelet_evict_frac_env_out_of_range_keeps_default() {
        let _g = env_guard();
        std::env::set_var(KUBELET_EVICT_PCT_ENV_VAR, "150");
        let frac = resolve_kubelet_evict_frac();
        std::env::remove_var(KUBELET_EVICT_PCT_ENV_VAR);
        assert_eq!(frac, DEFAULT_KUBELET_EVICT_PCT / 100.0);
    }

    #[test]
    fn sample_disk_reports_sane_mib_and_does_not_panic_computing_headroom() {
        // Emitting the headroom gauge shares the same disk_stat_bytes
        // probe as the mib pair; assert the mib numbers stay sane (the
        // gauge value itself needs a real Prometheus scrape / metrics
        // recorder to read back — an integration-level concern beyond
        // this pure probe test).
        let _g = env_guard();
        std::env::remove_var(KUBELET_EVICT_PCT_ENV_VAR);
        let dir = tempfile::tempdir().expect("tempdir");
        let probe = UtilizationProbe::new();
        let (total, used) = probe.sample_disk(dir.path());
        assert!(total > 0, "a real filesystem should report a total size");
        assert!(used <= total, "used must not exceed total");
    }

    #[test]
    fn first_cpu_sample_is_zero_then_subsequent_are_bounded() {
        let mut probe = UtilizationProbe::new();
        // First sample establishes the baseline → 0 regardless of platform.
        let first = probe.sample(Path::new("."), 0);
        assert_eq!(first.cpu_pct, 0.0);
        // Second sample must stay within [0, 100] on every platform
        // (0 on non-Linux where there's no /proc).
        let second = probe.sample(Path::new("."), 0);
        assert!((0.0..=100.0).contains(&second.cpu_pct));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reports_real_memory() {
        let (total, used) = mem_mib();
        assert!(total > 0, "Linux /proc/meminfo should report MemTotal");
        assert!(used <= total);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_meminfo_kb_extracts_value() {
        assert_eq!(parse_meminfo_kb("  32896180 kB"), 32_896_180);
        assert_eq!(parse_meminfo_kb(" garbage"), 0);
    }
}
