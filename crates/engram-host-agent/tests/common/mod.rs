//! Helpers shared by the host-agent's true-e2e FC integration tests
//! (`e2e_shell`, `e2e_vnc`). Lives at `tests/common/mod.rs` (cargo's standard
//! pattern for shared test-only code) so each `tests/<name>.rs` brings it in
//! via `mod common;`.
//!
//! Extracted from `e2e_shell.rs` when `e2e_vnc.rs` (ADR 0064) landed needing
//! the exact same preflight / root-check / host-state cleanup / guest-IP poll.
//! These four are the harness floor every "boot a real FC sandbox through
//! `PooledBackend`" test sits on; the per-test bake (ttyd vs the browser
//! bundle) stays in each test file because the rootfs shape differs.
//!
//! Linux-only, like the tests that use it: the whole crate's FC tests are
//! `#![cfg(target_os = "linux")]`, so this module never compiles on macOS.

#![allow(dead_code)] // Each test only uses a subset of helpers.

use std::path::PathBuf;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend; // brings `guest_ip` into scope
use engram_host_agent::pooled_backend::PooledBackend;
use tokio::time::sleep;

/// FC test artifact discovery — duplicates the helper in
/// `engram-sandbox-firecracker/tests/common/` because Rust can't share
/// `tests/common/` across crates and adding a workspace-member test crate just
/// for this is heavier than a 20-line inline copy.
pub struct FcEnv {
    pub kernel: PathBuf,
    #[allow(dead_code)]
    pub rootfs: PathBuf,
}

/// Skip cleanly unless `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` are set and exist.
/// Prints a `SKIP:` line and returns `None` so callers can
/// `let env = match common::fc_preflight() { Some(e) => e, None => return };`.
pub fn fc_preflight() -> Option<FcEnv> {
    let kernel = std::env::var("FC_TEST_KERNEL").ok()?;
    let rootfs = std::env::var("FC_TEST_ROOTFS").ok()?;
    let kp = PathBuf::from(&kernel);
    if !kp.exists() {
        eprintln!("SKIP: FC_TEST_KERNEL={kernel} doesn't exist");
        return None;
    }
    let rp = PathBuf::from(&rootfs);
    if !rp.exists() {
        eprintln!("SKIP: FC_TEST_ROOTFS={rootfs} doesn't exist");
        return None;
    }
    Some(FcEnv {
        kernel: kp,
        rootfs: rp,
    })
}

/// Skip unless running as root (these tests need CAP_NET_ADMIN for the TAP +
/// iptables that `host_startup` / warm-restore netns set up). Prints a `SKIP:`
/// line and returns `false` otherwise.
pub fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: FC e2e tests require root (CAP_NET_ADMIN for TAP + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Wipe stale engram-* iptables rules + tap-engr-/vh-engr- interfaces left from
/// prior runs. Borrowed verbatim from proxy_e2e's helpers so a crash mid-test
/// doesn't leak host state into the next run.
pub fn cleanup_host_state() {
    // iptables rules
    let saved = String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .unwrap_or_default();
    let mut current_table = "filter".to_string();
    for line in saved.lines() {
        if let Some(t) = line.strip_prefix('*') {
            current_table = t.trim().to_string();
            continue;
        }
        if !line.contains("engram-") {
            continue;
        }
        let Some(rest) = line.strip_prefix("-A ") else {
            continue;
        };
        let mut argv = vec!["-t".to_string(), current_table.clone(), "-D".to_string()];
        argv.extend(rest.split_whitespace().map(str::to_string));
        let _ = std::process::Command::new("iptables").args(&argv).output();
    }
    // TAP / veth interfaces
    for pat in ["tap-engr-", "vh-engr-"] {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ip -o link show | awk -F': ' '/{pat}/ {{print $2}}' | awk '{{print $1}}'"
            ))
            .output();
        let Ok(o) = out else { continue };
        for name in String::from_utf8_lossy(&o.stdout).lines() {
            let name = name.trim();
            if !name.is_empty() {
                let _ = std::process::Command::new("ip")
                    .args(["link", "delete", name])
                    .output();
            }
        }
    }
}

/// Wait up to `deadline` for `pooled.guest_ip(id)` to return Some. The FC
/// backend's `guest_ip` answers as soon as the in-VM agentd is reachable on
/// vsock; on a cold boot this typically takes a few seconds (kernel + init +
/// agentd). The poll cadence is fast (200ms) so the test isn't dominated by
/// sleep slack.
pub async fn wait_for_guest_ip(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    deadline: Duration,
) -> String {
    let start = std::time::Instant::now();
    loop {
        if let Some(ip) = pooled.guest_ip(id).await {
            return ip;
        }
        assert!(
            start.elapsed() < deadline,
            "guest_ip never resolved within {deadline:?}",
        );
        sleep(Duration::from_millis(200)).await;
    }
}
