//! Time and entropy as injected dependencies (ADR 0098 D1).
//!
//! The deterministic-simulation harness replaces the world behind
//! `Services` — and wall-clock time and randomness *are* world inputs, so
//! they get the same treatment as `MetadataStore` or `HostClient`: a trait
//! on `Services`, a production impl that hits the OS, and a simulated impl
//! (`engram-sim`) driven by the scheduler.
//!
//! Enforcement: the coordinator and postgres crates carry a `clippy.toml`
//! disallowing direct `chrono::Utc::now` / `Instant::now` /
//! `uuid::Uuid::new_v4` calls, and `just check` runs clippy with
//! `-D warnings`. [`SystemClock`] and [`OsEntropy`] below are the one
//! honest caller of each.

use std::fmt::Debug;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Wall-clock and monotonic time, plus async sleep.
///
/// Production: [`SystemClock`]. Simulation: `engram-sim`'s `SimClock`, a
/// view over tokio's paused clock advanced explicitly by the scheduler.
pub trait Clock: Send + Sync + Debug {
    /// Wall clock. Replaces every decision-feeding `Utc::now()`.
    fn now_utc(&self) -> DateTime<Utc>;

    /// Monotonic time since an arbitrary fixed per-process epoch.
    /// Replaces `Instant::now()`/`elapsed()` pairs — callers store the
    /// `Duration` mark and subtract, because a fake cannot mint opaque
    /// `Instant`s.
    fn now_mono(&self) -> Duration;

    /// Async sleep. Production: `tokio::time::sleep`. Under simulation
    /// the scheduler advances virtual time past due sleeps explicitly.
    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Randomness as a dependency: UUID minting and jitter.
///
/// Production: [`OsEntropy`]. Simulation: a seeded stream, so session /
/// sandbox / op ids replay from a seed.
pub trait Entropy: Send + Sync + Debug {
    /// Mint a UUID. Replaces inline `Uuid::new_v4()`.
    fn uuid(&self) -> uuid::Uuid;

    /// A value in `range`, for backoff jitter and sampling. Not for
    /// cryptographic use — key material stays on `engram-crypto`'s
    /// `OsRng` paths.
    fn u64(&self, range: Range<u64>) -> u64;
}

/// The production [`Clock`]: OS wall clock, a process-local monotonic
/// epoch, tokio sleep.
#[derive(Debug, Clone)]
pub struct SystemClock {
    birth: std::time::Instant,
}

impl SystemClock {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        #[allow(clippy::disallowed_methods)] // the one honest Instant::now caller
        Self {
            birth: std::time::Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    #[allow(clippy::disallowed_methods)] // the one honest Utc::now caller
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }

    #[allow(clippy::disallowed_methods)] // the one honest Instant::now caller
    fn now_mono(&self) -> Duration {
        self.birth.elapsed()
    }

    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(dur))
    }
}

/// The production [`Entropy`]: OS randomness via v4 UUIDs.
#[derive(Debug, Clone, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    #[allow(clippy::disallowed_methods)] // the one honest new_v4 caller
    fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::new_v4()
    }

    fn u64(&self, range: Range<u64>) -> u64 {
        assert!(!range.is_empty(), "empty entropy range");
        let bytes: [u8; 8] = self.uuid().as_bytes()[..8]
            .try_into()
            .expect("uuid has 16 bytes");
        let span = range.end - range.start;
        // Modulo bias is negligible for jitter/sampling (span << 2^64),
        // and this keeps the crate free of a rand dependency.
        range.start + (u64::from_le_bytes(bytes) % span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_mono_is_monotonic() {
        let clock = SystemClock::new();
        let a = clock.now_mono();
        let b = clock.now_mono();
        assert!(b >= a);
    }

    #[test]
    fn os_entropy_u64_stays_in_range() {
        let e = OsEntropy;
        for _ in 0..1000 {
            let v = e.u64(10..20);
            assert!((10..20).contains(&v));
        }
        assert_eq!(e.u64(7..8), 7);
    }
}
