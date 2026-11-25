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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use clock_steering::unix::UnixClock;
use statime_base::{Clock, Timestamp, TAI, UTC};

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
    phc: Option<UnixClock<TAI>>,
    /// Latched "the PHC stopped answering" state, so a persistent read
    /// failure logs on the way in and on the way out instead of once per
    /// [`TICK`]. See [`ClockSync::step_if_needed`] for why the transition
    /// — not the steady state — is the thing worth a log line.
    phc_degraded: AtomicBool,
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
                // WARN, not INFO: on FC this device is supposed to exist,
                // and without it the guest keeps whatever `CLOCK_REALTIME`
                // the snapshot froze — which silently breaks every TLS
                // handshake once the guest falls behind an upstream cert's
                // `notBefore`. The host-pushed `StepClock` is the backstop
                // (see `step_to`), but a guest that lost self-drive must
                // still be visible in its own log.
                tracing::warn!(
                    device = PTP_DEVICE,
                    error = ?e,
                    "no PTP host clock; guest clock self-sync disabled — \
                     clock corrections now depend entirely on the host's StepClock push"
                );
                None
            }
        };
        #[cfg(not(target_os = "linux"))]
        let phc: Option<UnixClock<TAI>> = None;
        Self {
            phc,
            phc_degraded: AtomicBool::new(false),
        }
    }

    fn step_if_needed(&self) {
        let Some(phc) = self.phc.as_ref() else {
            return;
        };
        let host = match phc.now() {
            Ok(t) => t,
            Err(e) => {
                // A device that opened but stopped answering is the shape
                // that stranded prod: the clock quietly stops tracking the
                // host and nothing says so. Log the TRANSITION at WARN —
                // the read is retried every TICK, so logging each failure
                // would emit 6/min forever and bury itself.
                if !self.phc_degraded.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        device = PTP_DEVICE,
                        error = ?e,
                        "PTP host clock stopped answering; guest clock is no longer \
                         tracking the host until this recovers"
                    );
                }
                return;
            }
        };
        if self.phc_degraded.swap(false, Ordering::Relaxed) {
            tracing::warn!(device = PTP_DEVICE, "PTP host clock recovered");
        }
        let sys = match UnixClock::CLOCK_REALTIME.now() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = ?e, "read system clock failed");
                return;
            }
        };
        // The host read is a `Timestamp<TAI>` (clock-steering tags the PTP
        // device timescale TAI generically) and the system read is
        // `Timestamp<UTC>`, so they can't be subtracted directly. But
        // `kvm_ptp` reports the host's `CLOCK_REALTIME` (wall time) and
        // `Clock::now` applies no conversion — the raw value is the same
        // nanosecond count either way. Rebase each on its own `UNIX_EPOCH`
        // (= 0) to get its raw count as a `Duration`, then subtract those.
        // `diff` is the signed offset to ADD to the system clock to reach the
        // host; `step_clock` splits the signed `Duration` itself (the old
        // in-tree `TimeOffset { seconds, nanos }` did that splitting by hand).
        let diff = (host - Timestamp::<TAI>::UNIX_EPOCH) - (sys - Timestamp::<UTC>::UNIX_EPOCH);
        if diff.as_nanos().abs() <= STEP_THRESHOLD_NANOS {
            return;
        }
        match UnixClock::CLOCK_REALTIME.step_clock(diff) {
            Ok(_) => tracing::info!(
                offset_secs = (diff.as_nanos() / NANOS_PER_SEC) as i64,
                "stepped guest clock to host PTP"
            ),
            Err(e) => tracing::warn!(
                error = ?e,
                "step guest clock failed (need CAP_SYS_TIME?)"
            ),
        }
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
        // `ClockSync::new` already warned with the open error on Linux; on
        // other targets there is no device to open and nothing to say.
        return;
    }
    sync_now(); // correct immediately on boot / first restore
    tokio::spawn(async {
        let mut tick = engram_core::periodic(TICK);
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
/// the host-pushed analogue of [`sync_now`]. A warm-restored VZ guest
/// wakes with its clock frozen at save time and has no PTP device at
/// all; the host sends `WireRequest::StepClock` with its own
/// `CLOCK_REALTIME` right after resume.
///
/// FC sends it as well, once per `start_agent` — the PTP self-drive
/// above is an optimization (it re-corrects continuously, for free),
/// but it is not something the host can verify, so the host asserts the
/// time itself before the harness starts. Both paths are idempotent and
/// share the [`STEP_THRESHOLD_NANOS`] policy, so whichever runs second
/// is a no-op.
///
/// Returns the applied offset in nanos, `None` when under threshold (or
/// on non-Linux, where there's no clock to step). Orthogonal to the
/// periodic PTP tick — this is a one-shot push, nothing to re-arm.
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
        // Host wall time as a UTC `Timestamp`, so the offset is the same
        // signed-`Duration` subtraction used in `step_if_needed`. `i64`
        // unix nanos is always ≥ 0 for any plausible host clock; the
        // euclidean split keeps `nanos` in 0..1e9 regardless.
        let host = Timestamp::from_seconds_nanos_since_unix_epoch(
            host_unix_nanos.div_euclid(1_000_000_000_i64) as u64,
            host_unix_nanos.rem_euclid(1_000_000_000_i64) as u32,
        );
        let diff = host - sys;
        if diff.as_nanos().abs() <= STEP_THRESHOLD_NANOS {
            return None;
        }
        let applied = diff.as_nanos();
        match UnixClock::CLOCK_REALTIME.step_clock(diff) {
            Ok(_) => {
                tracing::info!(
                    offset_secs = (applied / NANOS_PER_SEC) as i64,
                    "stepped guest clock to host-supplied time (StepClock)"
                );
                Some(applied as i64)
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

    /// The signed `host - sys` offset the new `Timestamp` subtraction gives,
    /// expressed in nanos — the input to the `STEP_THRESHOLD_NANOS` check and
    /// to `step_clock`. These pin the behavior the old `offset_nanos` +
    /// `to_time_offset` helpers produced, now delegated to `statime_base`.
    #[test]
    fn positive_offset_is_additive_nanos() {
        // host 12h ahead of sys → +43200s.
        let host = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(1_000_043_200, 0);
        let sys = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(1_000_000_000, 0);
        assert_eq!((host - sys).as_nanos(), 43_200 * NANOS_PER_SEC);
    }

    #[test]
    fn negative_offset_holds_sign() {
        // host 1.5s BEHIND sys → -1.5e9 ns.
        let host = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(100, 0);
        let sys = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(101, 500_000_000);
        assert_eq!((host - sys).as_nanos(), -1_500_000_000);
    }

    #[test]
    fn sub_second_fraction_preserved() {
        let host = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(10, 250_000_000);
        let sys = Timestamp::<UTC>::from_seconds_nanos_since_unix_epoch(5, 0);
        assert_eq!((host - sys).as_nanos(), 5 * NANOS_PER_SEC + 250_000_000);
    }
}
