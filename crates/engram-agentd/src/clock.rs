//! Keep the guest wall clock synced to the host across FC snapshot/restore.
//!
//! Firecracker snapshot/restore freezes `CLOCK_REALTIME` at capture time,
//! so a session restored long after its snapshot wakes up with a clock
//! hours behind real time. That breaks anything with a narrow time window
//! — AWS SigV4 (sccache → GCS), short-lived token `nbf`/`exp` checks,
//! correct log timestamps. The guest kernel exposes the host clock via the
//! KVM PTP device (`/dev/ptp0`); we step `CLOCK_REALTIME` toward it.
//!
//! [`clock_steering`] (the clock layer behind `ntpd-rs`) owns the gnarly
//! parts — reading the PHC and the `adjtimex` step. We own only the
//! policy: step when the offset exceeds a threshold, on a periodic tick
//! AND right before every exec / harness spawn — so a just-resumed
//! session's child starts on a correct clock (zero window). The skew is
//! re-introduced on *every* resume (each freezes the clock for the idle
//! duration), so a one-shot sync wouldn't survive an idle→resume cycle;
//! hence the per-spawn + periodic discipline.

use std::sync::OnceLock;
use std::time::Duration;

use clock_steering::{unix::UnixClock, Clock, TimeOffset};

/// `kvm_ptp` exposes the host's `CLOCK_REALTIME` here.
const PTP_DEVICE: &str = "/dev/ptp0";

/// Step only when off by more than this. SigV4 allows ~15 min; a 2 s
/// floor stays far inside that while ignoring sub-second jitter and not
/// stepping needlessly in steady state.
const STEP_THRESHOLD_NANOS: i128 = 2 * NANOS_PER_SEC;

/// Background re-check cadence. Covers re-resumes (each restore re-freezes
/// the clock) and slow drift for long-lived children (the harness); the
/// pre-spawn sync is what makes fresh execs window-free.
const TICK: Duration = Duration::from_secs(10);

const NANOS_PER_SEC: i128 = 1_000_000_000;

static CLOCK: OnceLock<ClockSync> = OnceLock::new();

struct ClockSync {
    /// The KVM PTP clock (= host wall time). `None` when the device isn't
    /// present (non-FC backends / dev hosts / tests) — then every op is a
    /// no-op and the guest clock is left untouched.
    phc: Option<UnixClock>,
}

impl ClockSync {
    fn new() -> Self {
        // Opening the PTP device is Linux-only (it's a KVM-PTP char
        // device); the real guest target is Linux. On other targets
        // (macOS dev builds) there's no host PHC, so sync is disabled and
        // every op is a no-op.
        #[cfg(target_os = "linux")]
        let phc = match UnixClock::open(PTP_DEVICE) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::info!(
                    device = PTP_DEVICE,
                    error = ?e,
                    "no PTP host clock; guest clock sync disabled"
                );
                None
            }
        };
        #[cfg(not(target_os = "linux"))]
        let phc: Option<UnixClock> = None;
        Self { phc }
    }

    fn step_if_needed(&self) {
        let Some(phc) = self.phc.as_ref() else {
            return;
        };
        let host = match phc.now() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = ?e, "read PTP host clock failed");
                return;
            }
        };
        let sys = match UnixClock::CLOCK_REALTIME.now() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = ?e, "read system clock failed");
                return;
            }
        };
        let diff = offset_nanos(
            (host.seconds as i128, host.nanos),
            (sys.seconds as i128, sys.nanos),
        );
        if diff.abs() <= STEP_THRESHOLD_NANOS {
            return;
        }
        match UnixClock::CLOCK_REALTIME.step_clock(to_time_offset(diff)) {
            Ok(_) => tracing::info!(
                offset_secs = (diff / NANOS_PER_SEC) as i64,
                "stepped guest clock to host PTP"
            ),
            Err(e) => tracing::warn!(
                error = ?e,
                "step guest clock failed (need CAP_SYS_TIME?)"
            ),
        }
    }
}

/// Signed nanoseconds to ADD to `sys` to reach `host`. Each arg is
/// `(seconds, nanos)`.
fn offset_nanos(host: (i128, u32), sys: (i128, u32)) -> i128 {
    (host.0 * NANOS_PER_SEC + host.1 as i128) - (sys.0 * NANOS_PER_SEC + sys.1 as i128)
}

/// Normalize a signed ns offset into `clock-steering`'s [`TimeOffset`]
/// (seconds floored, nanos in `0..1e9` — `div_euclid`/`rem_euclid` keep
/// `nanos` non-negative even for a negative offset).
fn to_time_offset(diff_ns: i128) -> TimeOffset {
    TimeOffset {
        seconds: diff_ns.div_euclid(NANOS_PER_SEC) as _,
        nanos: diff_ns.rem_euclid(NANOS_PER_SEC) as u32,
    }
}

/// Open the PTP device and, when present, correct the clock immediately
/// and spawn the periodic stepping loop. Call once at agentd startup,
/// inside the tokio runtime. A `ClockSync` is installed even without a
/// device so [`sync_now`] stays branchless + cheap at the call sites.
pub fn init() {
    let cs = ClockSync::new();
    let enabled = cs.phc.is_some();
    let _ = CLOCK.set(cs);
    if !enabled {
        return;
    }
    sync_now(); // correct immediately on boot / first restore
    tokio::spawn(async {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            sync_now();
        }
    });
    tracing::info!(
        device = PTP_DEVICE,
        tick_secs = TICK.as_secs(),
        "guest clock sync enabled"
    );
}

/// Step the clock now if it's drifted past the threshold. Cheap (two
/// clock reads; a step only when needed). Called before every exec /
/// harness spawn so a child starts on a correct clock even immediately
/// after a resume. No-op until [`init`] runs / when no PTP device exists.
pub fn sync_now() {
    if let Some(cs) = CLOCK.get() {
        cs.step_if_needed();
    }
}

/// ADR 0096 D7: step `CLOCK_REALTIME` to a host-supplied wall clock —
/// the host-pushed analogue of [`sync_now`] for guests with NO PTP
/// device (VZ). A warm-restored VZ guest wakes with its clock frozen at
/// save time; the host sends `WireRequest::StepClock` with its own
/// `CLOCK_REALTIME` right after resume. Same policy as the PTP path:
/// step only past [`STEP_THRESHOLD_NANOS`]. Returns the applied offset
/// in nanos, `None` when under threshold (or on non-Linux, where
/// there's no clock to step). Orthogonal to the periodic PTP tick —
/// this is a one-shot push, nothing to re-arm.
pub fn step_to(host_unix_nanos: i64) -> Option<i64> {
    #[cfg(target_os = "linux")]
    {
        let sys = match UnixClock::CLOCK_REALTIME.now() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = ?e, "read system clock failed");
                return None;
            }
        };
        let diff =
            host_unix_nanos as i128 - (sys.seconds as i128 * NANOS_PER_SEC + sys.nanos as i128);
        if diff.abs() <= STEP_THRESHOLD_NANOS {
            return None;
        }
        match UnixClock::CLOCK_REALTIME.step_clock(to_time_offset(diff)) {
            Ok(_) => {
                tracing::info!(
                    offset_secs = (diff / NANOS_PER_SEC) as i64,
                    "stepped guest clock to host-supplied time (StepClock)"
                );
                Some(diff as i64)
            }
            Err(e) => {
                tracing::warn!(error = ?e, "StepClock step failed (need CAP_SYS_TIME?)");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = host_unix_nanos;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_offset_normalizes() {
        // host 12h ahead of sys → +43200s, 0ns.
        let diff = offset_nanos((1_000_043_200, 0), (1_000_000_000, 0));
        assert_eq!(diff, 43_200 * NANOS_PER_SEC);
        let off = to_time_offset(diff);
        assert_eq!(off.seconds as i128, 43_200);
        assert_eq!(off.nanos, 0);
    }

    #[test]
    fn negative_offset_floors_with_nonnegative_nanos() {
        // host 1.5s BEHIND sys → diff = -1.5e9 → seconds=-2, nanos=5e8.
        let diff = offset_nanos((100, 0), (101, 500_000_000));
        assert_eq!(diff, -1_500_000_000);
        let off = to_time_offset(diff);
        assert_eq!(off.seconds as i128, -2);
        assert_eq!(off.nanos, 500_000_000);
        // re-expanding gives back the original diff (no information lost).
        assert_eq!(
            off.seconds as i128 * NANOS_PER_SEC + off.nanos as i128,
            diff
        );
    }

    #[test]
    fn sub_second_fraction_preserved() {
        let diff = offset_nanos((10, 250_000_000), (5, 0));
        assert_eq!(diff, 5 * NANOS_PER_SEC + 250_000_000);
        let off = to_time_offset(diff);
        assert_eq!(off.seconds as i128, 5);
        assert_eq!(off.nanos, 250_000_000);
    }
}
