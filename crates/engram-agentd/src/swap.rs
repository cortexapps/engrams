//! ADR 0112: arm the guest's ephemeral swap device.
//!
//! The host attaches at most one WRITABLE non-vda virtio disk — the
//! swap drive (`SandboxSpec.swap_mib`; every aux bundle attaches
//! read-only, and vda is the rootfs). This module finds it, runs
//! `mkswap` + `swapon`, and applies the reclaim sysctls sized for a
//! guest whose file pages live on chunked-NBD backing.
//!
//! Called from two places, both best-effort (log-and-continue — swap
//! must never block agent bring-up, and a swapless guest is merely
//! the pre-0112 behavior):
//! - **Cold boot** (`main.rs`, beside `tuning::apply_block_readahead`)
//!   — the base-capture VM and rung-2 disk-only recovery boots.
//! - **Session bind** (`harness_supervisor::spawn`, beside
//!   `remount_and_log`) — a restored guest meets its FRESH zero-filled
//!   backing here (capture ran `swapoff` before the pause, restore
//!   re-pointed the device at a new sparse file), so the full
//!   `mkswap` + `swapon` re-arm runs again.
//!
//! Kill switch: `ENGRAM_GUEST_SWAP=off` in agentd's environment makes
//! every arm a no-op — the device stays attached and unused, which is
//! safe, and the flag reaches sessions through an agentd roll with no
//! image re-capture.

use std::path::{Path, PathBuf};

/// `vm.*` sysctls applied when (and only when) a swap device is armed.
///
/// - `swappiness=100` (kernel 6.1 range 0-200): "anonymous and file
///   reclaim cost the same." Post-#1044 that is approximately true
///   (ranged-pread refault ≈ swap-in on the same NVMe), and the
///   measured incident (session f660f022, 22:1 scan-to-steal) was the
///   default 60 forcing reclaim onto hot NBD-backed file pages because
///   anon was unreclaimable.
/// - `page-cluster=0`: swap readahead is 2^n pages; swap-in here is
///   random 4 KiB against fast backing — cluster reads only amplify
///   I/O during exactly the reclaim storms swap exists to survive.
/// - `watermark_scale_factor=125` (default 10): wake kswapd earlier;
///   the incident's 22:1 direct-reclaim ratio is kswapd waking too
///   late and allocators paying the reclaim inline.
const VM_SYSCTLS: [(&str, &str); 3] = [
    ("/proc/sys/vm/swappiness", "100"),
    ("/proc/sys/vm/page-cluster", "0"),
    ("/proc/sys/vm/watermark_scale_factor", "125"),
];

/// Kill-switch env var. `off` (case-insensitive) disables arming — and
/// at bind, actively DISARMS an already-armed guest. Two carriers:
/// agentd's process env (frozen into the base snapshot at capture —
/// boot-time only) and the spawn-delivered session/image env (built by
/// the coordinator from the CURRENT config at bind time — this is the
/// one that reaches existing sessions with no re-capture).
pub const GUEST_SWAP_ENV: &str = "ENGRAM_GUEST_SWAP";

/// Is the kill switch thrown, given the highest-precedence value that
/// carries it? Pure for tests.
fn switched_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("off"))
}

/// Boot-time arm (cold boots: base capture, rung-2 recovery). Only the
/// process env can carry the switch here — no session is bound yet.
pub fn arm() {
    let process_off = std::env::var(GUEST_SWAP_ENV).ok();
    if switched_off(process_off.as_deref()) {
        tracing::info!("guest swap disabled via {GUEST_SWAP_ENV}=off (process env); not arming");
        return;
    }
    arm_inner();
}

/// Bind-time arm (every `SpawnHarness`, including the readiness
/// probe). `env` is the spawn-delivered session∪spawn env — the
/// coordinator builds it from the CURRENT image/session config, so a
/// config-level `ENGRAM_GUEST_SWAP=off` reaches this session at its
/// next bind with no re-capture, and actively DISARMS a guest that is
/// already armed (the emergency-rollback posture: the device stays
/// attached and unused, which is safe).
pub fn arm_at_bind(env: &std::collections::HashMap<String, String>) {
    let bind_off = env.get(GUEST_SWAP_ENV).map(String::as_str);
    let process_off = std::env::var(GUEST_SWAP_ENV).ok();
    if switched_off(bind_off) || switched_off(process_off.as_deref()) {
        // Explicit device, NOT `swapoff -a`: busybox's `-a` reads
        // /etc/fstab only — it errors on a missing fstab and silently
        // disarms nothing when one exists, since our swap is armed by
        // explicit `swapon /dev/vdX`, never an fstab entry.
        match find_swap_device(Path::new("/sys/block")) {
            Some(dev) if swap_is_active(Path::new("/proc/swaps"), &dev) == Some(true) => {
                tracing::info!(
                    dev,
                    "guest swap disabled via {GUEST_SWAP_ENV}=off; disarming",
                );
                let node = format!("/dev/{dev}");
                if !run_logged("swapoff", &[&node]) {
                    tracing::warn!(
                        dev,
                        "kill-switch swapoff failed; swap may remain armed until retry",
                    );
                }
            }
            _ => tracing::info!(
                "guest swap disabled via {GUEST_SWAP_ENV}=off; nothing armed to disarm",
            ),
        }
        return;
    }
    arm_inner();
}

/// Arm swap end to end: find the device, skip if already active,
/// `mkswap` + `swapon`, then apply the reclaim sysctls. Synchronous
/// (call via `spawn_blocking` from async contexts); every failure
/// logs and returns — never an error.
fn arm_inner() {
    let Some(dev) = find_swap_device(Path::new("/sys/block")) else {
        // No writable non-vda disk ⇒ the image doesn't opt into swap.
        return;
    };
    if swap_is_active(Path::new("/proc/swaps"), &dev) == Some(true) {
        // Mid-residence re-bind (e.g. a second SpawnHarness): the
        // device is already armed; mkswap over live swap would be
        // destructive and swapon would EBUSY. `None` (unreadable
        // /proc/swaps) falls through — mkswap/swapon speak for
        // themselves.
        tracing::debug!(dev, "swap already active; skipping re-arm");
        return;
    }
    let node = format!("/dev/{dev}");
    // A fresh backing is zero-filled (no signature), and a
    // mid-residence swapoff'd device still carries one — mkswap
    // unconditionally so both cases converge on a known-good header.
    if !run_logged("mkswap", &[&node]) {
        return;
    }
    if !run_logged("swapon", &[&node]) {
        return;
    }
    for (path, value) in VM_SYSCTLS {
        match std::fs::write(path, value) {
            Ok(()) => tracing::info!(path, value, "vm sysctl set"),
            Err(error) => tracing::warn!(path, value, %error, "vm sysctl not applied"),
        }
    }
    tracing::info!(dev, "guest swap armed");
}

/// The swap device is the only WRITABLE non-vda virtio disk (aux
/// bundles attach `is_read_only: true`; vda is the rootfs). Returns
/// its name (`vdX`). Split out over a sysfs root for tests.
pub fn find_swap_device(sys_block: &Path) -> Option<String> {
    let entries = std::fs::read_dir(sys_block).ok()?;
    let mut candidates: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !name.starts_with("vd") || name == "vda" {
                return None;
            }
            let ro = std::fs::read_to_string(e.path().join("ro")).ok()?;
            (ro.trim() == "0").then_some(name)
        })
        .collect();
    match candidates.len() {
        0 => None,
        1 => candidates.pop(),
        // More than one writable non-vda disk breaks the identity rule
        // this module depends on — refuse to guess (mkswap over the
        // wrong device is destructive).
        n => {
            tracing::warn!(
                candidates = ?candidates,
                "expected at most one writable non-vda disk, found {n}; not arming swap",
            );
            None
        }
    }
}

/// Is `dev` already an active swap area, per `/proc/swaps`? `None` =
/// unreadable (treat as inactive; the arm's mkswap/swapon will speak
/// for themselves).
fn swap_is_active(proc_swaps: &Path, dev: &str) -> Option<bool> {
    let raw = std::fs::read_to_string(proc_swaps).ok()?;
    let node = PathBuf::from("/dev").join(dev);
    Some(
        raw.lines()
            .skip(1) // header
            .any(|l| l.split_whitespace().next() == node.to_str()),
    )
}

/// Run a command to completion; log the outcome, return success.
fn run_logged(cmd: &str, args: &[&str]) -> bool {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => {
            tracing::info!(cmd, ?args, "swap arm step ok");
            true
        }
        Ok(out) => {
            tracing::warn!(
                cmd,
                ?args,
                status = %out.status,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "swap arm step failed",
            );
            false
        }
        Err(error) => {
            tracing::warn!(cmd, ?args, %error, "swap arm step could not spawn");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_dev(root: &Path, name: &str, ro: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("ro"), ro).unwrap();
    }

    #[test]
    fn picks_the_only_writable_non_vda_disk() {
        let tmp = tempfile::tempdir().unwrap();
        mk_dev(tmp.path(), "vda", "0"); // rootfs: writable but excluded
        mk_dev(tmp.path(), "vdb", "1"); // aux bundle: RO
        mk_dev(tmp.path(), "vdc", "1"); // aux bundle: RO
        mk_dev(tmp.path(), "vdd", "0"); // the swap drive
        assert_eq!(find_swap_device(tmp.path()).as_deref(), Some("vdd"));
    }

    #[test]
    fn no_swap_drive_means_none() {
        let tmp = tempfile::tempdir().unwrap();
        mk_dev(tmp.path(), "vda", "0");
        mk_dev(tmp.path(), "vdb", "1");
        assert_eq!(find_swap_device(tmp.path()), None);
        // Empty / missing sysfs (non-Linux tests) is also None.
        assert_eq!(find_swap_device(&tmp.path().join("missing")), None);
    }

    #[test]
    fn two_writable_candidates_refuse_to_guess() {
        let tmp = tempfile::tempdir().unwrap();
        mk_dev(tmp.path(), "vda", "0");
        mk_dev(tmp.path(), "vdb", "0");
        mk_dev(tmp.path(), "vdc", "0");
        assert_eq!(find_swap_device(tmp.path()), None);
    }

    #[test]
    fn non_virtio_devices_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        mk_dev(tmp.path(), "sda", "0");
        mk_dev(tmp.path(), "nbd0", "0");
        mk_dev(tmp.path(), "loop0", "0");
        assert_eq!(find_swap_device(tmp.path()), None);
    }

    #[test]
    fn kill_switch_matches_off_case_insensitively_and_nothing_else() {
        assert!(switched_off(Some("off")));
        assert!(switched_off(Some("OFF")));
        assert!(switched_off(Some("Off")));
        // Anything that isn't `off` leaves swap armed — including
        // typos and truthy-looking values (the switch is a kill
        // switch, not a tristate).
        assert!(!switched_off(Some("on")));
        assert!(!switched_off(Some("0")));
        assert!(!switched_off(Some("false")));
        assert!(!switched_off(Some("")));
        assert!(!switched_off(None));
    }

    #[test]
    fn proc_swaps_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let swaps = tmp.path().join("swaps");
        std::fs::write(
            &swaps,
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
             /dev/vdd                                partition\t65532\t\t0\t\t-2\n",
        )
        .unwrap();
        assert_eq!(swap_is_active(&swaps, "vdd"), Some(true));
        assert_eq!(swap_is_active(&swaps, "vdc"), Some(false));
        assert_eq!(swap_is_active(&tmp.path().join("missing"), "vdd"), None);
    }
}
