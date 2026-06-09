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
