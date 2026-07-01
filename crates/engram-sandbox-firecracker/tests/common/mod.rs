//! Helpers shared by every FC integration test. Lives at
//! `tests/common/mod.rs` (cargo's standard pattern for shared
//! test-only code) so each `tests/<name>.rs` brings it in via
//! `mod common;`.
//!
//! Five tests live in this directory and they all need the same four
//! things: skip cleanly when the host can't run Firecracker, find a
//! binary on `$PATH`, drain an `ExecStream` into stdout/stderr/exit,
//! and resolve the cached test artifacts. Fifth duplicate prompted
//! the extraction.

#![allow(dead_code)] // Each test only uses a subset of helpers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engram_core::types::sandbox::ExecEvent;
use futures::StreamExt;

/// Successful preflight: paths to the cached vmlinux + ext4 rootfs.
pub struct FcEnv {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
}

/// Verify the host can drive Firecracker for an `#[ignore]`'d test:
/// `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` env vars set, `/dev/kvm`
/// present, `firecracker` on `$PATH`. Prints a `SKIP:` line and
/// returns `None` on the first missing prereq so the test can
/// `let env = match common::fc_preflight() { Some(e) => e, None => return };`
/// without growing per-test boilerplate.
///
/// Tests that need additional binaries (e.g. `docker`, `mke2fs`)
/// follow up with [`require_bin`].
pub fn fc_preflight() -> Option<FcEnv> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    if which("firecracker").is_none() {
        eprintln!("SKIP: firecracker binary not on PATH");
        return None;
    }
    Some(FcEnv { kernel, rootfs })
}

/// Successful compat preflight: the cached artifacts plus the two
/// Firecracker binaries (stock + fork) to round-trip a snapshot between.
pub struct CompatEnv {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub stock_bin: PathBuf,
    pub fork_bin: PathBuf,
}

/// Like [`fc_preflight`] but for the stock↔fork snapshot-compat test
/// (ADR 0045 Phase B): needs `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` +
/// `/dev/kvm`, plus BOTH `ENGRAM_FC_STOCK_BIN` and `ENGRAM_FC_FORK_BIN`
/// pointing at the two binaries to interoperate. No `firecracker`-on-`$PATH`
/// requirement: the test sets `FirecrackerConfig::firecracker_bin` per
/// backend. Prints `SKIP:` and returns `None` on the first missing prereq.
pub fn compat_preflight() -> Option<CompatEnv> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    let stock_bin = match std::env::var("ENGRAM_FC_STOCK_BIN") {
        Ok(p) if Path::new(&p).is_file() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_STOCK_BIN not set to an existing stock firecracker binary");
            return None;
        }
    };
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if Path::new(&p).is_file() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set to an existing forked firecracker binary");
            return None;
        }
    };
    Some(CompatEnv {
        kernel,
        rootfs,
        stock_bin,
        fork_bin,
    })
}

/// Returns `false` (after printing `SKIP:`) if `bin` isn't on `$PATH`.
/// Used by tests with extra binary requirements (docker, mke2fs).
pub fn require_bin(bin: &str) -> bool {
    if which(bin).is_none() {
        eprintln!("SKIP: {bin} not on PATH");
        return false;
    }
    true
}

/// Drain an `ExecStream`'s events into separated stdout/stderr buffers
/// plus the terminal exit code. Stops at the first `Exit` event.
pub async fn drain(
    mut stream: impl StreamExt<Item = ExecEvent> + Unpin,
) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some(ev) = stream.next().await {
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(code) => {
                exit_code = code;
                break;
            }
        }
    }
    (stdout, stderr, exit_code)
}

/// Tiny `which`: walk `$PATH`, return the first match. Avoids pulling
/// the `which` crate as a dev-dep.
pub fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|p| Path::new(p).join(bin))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Readiness polling.
//
// These tests boot real microVMs, but a microVM boots in ~0.3s and a
// snapshot/restore round-trips in ~1s. The slow way to wait for that work is
// a fixed `sleep` sized for the worst case; the fast way is to poll the
// observable the test already cares about and return the instant it holds.
// `boot.rs` proves the pattern (it polls the API socket and finishes in
// 0.34s). The helpers below generalize it so the rest of the suite can stop
// sleeping. Bound the worst case with a generous ceiling; pay only the real
// latency in the common case.
//
// NB: most of these guests boot the public ubuntu rootfs to `init=/bin/bash`
// with no in-guest agentd, so `wait_agent_ready` is not available — the
// host-observable signals are the FC API socket, the serial console (funneled
// to a log file), and `/proc` gauges.
// ---------------------------------------------------------------------------

/// Poll a synchronous predicate every `interval` until it returns `true`, or
/// `timeout` elapses. Returns whether the condition was met. The drop-in
/// replacement for a fixed `sleep` whose purpose was "wait for X to become
/// true" where X is a cheap fs / `/proc` / in-memory check.
pub async fn poll_until(
    timeout: Duration,
    interval: Duration,
    mut pred: impl FnMut() -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// [`poll_until`] for an async predicate — e.g. a `backend.list()`-shaped
/// condition that has to `.await`.
pub async fn poll_until_async<F, Fut>(timeout: Duration, interval: Duration, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Poll until `path` (a Firecracker API socket) is bound, or `timeout`
/// elapses. Promoted from `boot.rs` so the raw-`spawn_firecracker` tests can
/// drop their fixed post-spawn sleeps. 50ms ticks.
pub async fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    poll_until(timeout, Duration::from_millis(50), || path.exists()).await
}

/// Poll `path` until its contents contain ANY of `needles`, or `timeout`
/// elapses. Returns the final contents (a missing file reads as empty) so the
/// caller can build a useful assertion message on a miss.
///
/// Firecracker funnels the guest serial console (`console=ttyS0`) plus its own
/// stderr into one log file — `firecracker.log` under the jail dir for
/// backend-managed VMs, or the file the raw `spawn_firecracker` tests redirect
/// into — so a kernel- or guest-emitted marker is observable here without an
/// in-guest agent.
pub async fn wait_for_log_contains(path: &Path, needles: &[&str], timeout: Duration) -> String {
    let mut last = String::new();
    let met = poll_until(timeout, Duration::from_millis(100), || {
        last = std::fs::read_to_string(path).unwrap_or_default();
        needles.iter().any(|n| last.contains(n))
    })
    .await;
    let _ = met;
    last
}

/// Like [`wait_for_log_contains`], but waits until `needle` appears at least
/// `count` times — the post-resume read-loop assertions count distinct marker
/// lines. Returns the final contents.
pub async fn wait_for_log_count(
    path: &Path,
    needle: &str,
    count: usize,
    timeout: Duration,
) -> String {
    let mut last = String::new();
    let met = poll_until(timeout, Duration::from_millis(100), || {
        last = std::fs::read_to_string(path).unwrap_or_default();
        last.matches(needle).count() >= count
    })
    .await;
    let _ = met;
    last
}
