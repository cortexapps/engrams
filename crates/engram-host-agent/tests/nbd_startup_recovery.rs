//! ADR 0017 Phase B integration test: `recover_stuck_nbd_devices`
//! exercised against real Linux `/dev/nbdN` devices.
//!
//! Two flavors:
//!
//! 1. `recovery_is_noop_for_unbound_devices` — runs unconditionally;
//!    feeds the helper a list of devices known to be unbound (or
//!    nonexistent) and asserts `(probed=0, recovered=0, stuck=0)`.
//!    This is the always-on smoke test that doesn't need root or a
//!    pre-stuck environment.
//!
//! 2. `recovery_clears_kernel_busy_device` — `#[ignore]`'d; needs a
//!    stuck NBD device staged ahead of time (run-by-hand on dev-vm
//!    after an ungraceful host-agent exit; OR, in CI, ENGRAM_NBD_STUCK
//!    DEVICES=/dev/nbdX env var points at one). Asserts the helper
//!    reports `recovered >= 1` and `/sys/block/nbdX/pid` is cleared
//!    afterwards. (ADR 0044 K2: recovery is netlink
//!    `NBD_CMD_DISCONNECT` now, and the caller scopes the sweep to
//!    slots NOT claimed by surviving sandboxes — a survivor's busy
//!    device is alive by design.)

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use engram_host_agent::disk_daemon::{recover_stuck_nbd_devices, NbdSlotAllocator};

#[tokio::test]
async fn recovery_is_noop_for_unbound_devices() {
    // Devices that don't exist (or aren't bound to any NBD daemon)
    // should probe as not-stuck → no work. The candidate paths aren't
    // in the pool's universe, so `try_claim` skips them (the sweep only
    // touches slots it can claim) and nothing is probed.
    let pool = NbdSlotAllocator::from_paths(vec![PathBuf::from("/dev/nbd0")])
        .expect("build single-slot pool");
    let paths = vec![
        PathBuf::from("/dev/test-fake-nbd-recovery-0"),
        PathBuf::from("/dev/test-fake-nbd-recovery-1"),
    ];
    let (probed, recovered, stuck) = recover_stuck_nbd_devices(&pool, &paths).await;
    assert_eq!(probed, 0, "fake paths should not register as probed");
    assert_eq!(recovered, 0);
    assert_eq!(stuck, 0);
}

/// ADR 0017 Phase B end-to-end: confirms the recovery actually
/// releases a real stuck device. Run via:
///
/// ```sh
/// # On dev-vm, with at least one stuck /dev/nbdN bound to a dead PID:
/// ENGRAM_NBD_STUCK_DEVICES=/dev/nbd1,/dev/nbd2 \
///   cargo test -p engram-host-agent --test nbd_startup_recovery \
///     recovery_clears_kernel_busy_device -- --ignored --nocapture
/// ```
///
/// Skips cleanly if the env var is unset or no listed device is
/// actually stuck — so it's safe to leave in CI but only fires when
/// the operator deliberately stages the scenario.
#[tokio::test]
#[ignore]
async fn recovery_clears_kernel_busy_device() {
    let env = match std::env::var("ENGRAM_NBD_STUCK_DEVICES") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!("ENGRAM_NBD_STUCK_DEVICES unset; skipping");
            return;
        }
    };
    let paths: Vec<PathBuf> = env
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    assert!(!paths.is_empty(), "ENGRAM_NBD_STUCK_DEVICES had no entries");

    let initial_busy: Vec<bool> = paths.iter().map(|p| pid_file_populated(p)).collect();
    let initial_busy_count = initial_busy.iter().filter(|b| **b).count();
    if initial_busy_count == 0 {
        eprintln!(
            "no listed devices were actually kernel-busy at start; nothing to recover. Skipping."
        );
        return;
    }

    // Recovery requires R/W access to `/dev/nbdN` (the open(2) inside
    // recover_one_stuck_device). Production hosts grant this via a
    // udev rule installed by the Packer manifest; tests run as a
    // non-root user that may not be in the `disk` group. Skip rather
    // than fail when the device file is unreadable — the ioctl path
    // is structurally identical regardless of whether the caller
    // happens to have permission, and the noop unit test plus this
    // test's "no busy devices → skip" branch are still meaningful.
    let any_openable = paths.iter().filter(|p| pid_file_populated(p)).any(|p| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)
            .is_ok()
    });
    if !any_openable {
        eprintln!("no stuck device is opened R/W by this user; need disk-group or root. Skipping.");
        return;
    }

    // Build a pool whose universe is exactly the candidate devices so
    // the sweep can `try_claim` each one (the claim-then-disconnect
    // path). A device kernel-busy under a DEAD pid is "free" to the
    // pool's reserved-bit accounting, so try_claim reserves it.
    let pool = NbdSlotAllocator::from_paths(paths.clone()).expect("build pool over stuck devices");
    let (probed, recovered, _stuck) = recover_stuck_nbd_devices(&pool, &paths).await;
    assert_eq!(
        probed, initial_busy_count,
        "probed count must match the # of initially-busy devices"
    );
    assert!(
        recovered > 0,
        "expected to recover at least one stuck device; nothing released"
    );

    // Post-recovery: at least one previously-busy device must now be
    // free. We don't insist on "all recovered" because the kernel can
    // refuse to release a device in rare cases (reboot required).
    let post_busy_count = paths.iter().filter(|p| pid_file_populated(p)).count();
    assert!(
        post_busy_count < initial_busy_count,
        "recovery reported {recovered} freed but pid files still show {post_busy_count} busy (was {initial_busy_count})",
    );
}

fn pid_file_populated(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let pid_path = format!("/sys/block/{name}/pid");
    matches!(std::fs::read_to_string(&pid_path), Ok(s) if !s.trim().is_empty())
}
