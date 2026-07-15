//! ADR 0094: the resume storm-shield.
//!
//! A cold harness spawn on a just-resumed guest races the warm stack's
//! wake-up stampede (JVM GC catch-up, health-check retries, timer
//! floods) — measured 39–41 s to first reply vs **2.7 s on a quiet
//! guest**, and neither nice (engrams#669) nor the memory substrate
//! (ADR 0092: File-mode ≈ UFFD ≈ 45 s) moves it, because the contention
//! is vCPU saturation + the virtio/NBD disk queue. The one lever that
//! provably works is sequencing: SIGSTOP the resumed workload, spawn
//! the harness onto the quiet guest, SIGCONT everything a few seconds
//! later. The stampede still happens — overlapped with model latency
//! instead of ahead of the first token.
//!
//! Trigger: the harness supervisor's cold-spawn arm calls [`engage`]
//! when the host set `post_restore` on the `SpawnHarness` frame. The
//! host is the only party that reliably knows a spawn follows a restore
//! — a guest-side clock-step heuristic was tried and **fails on fresh
//! creates** (dev-VM-proven 2026-07-15): the captured agentd corrects
//! the clock during ITS boot, before the ADR 0080 RefreshAgent re-exec
//! loads the new agentd, so the new agentd never observes the skew. The
//! reattach arm returns before [`engage`], so a live re-issue of a
//! still-running harness never freezes.
//!
//! Fail-open: freeze/thaw are best-effort per pid, the thaw runs from a
//! detached timer AND from guard drop (spawn failure), and a startup
//! backstop ([`startup_audit`]) SIGCONTs any state-`T` process no active
//! shield owns — healing a freeze that outlived its agentd (captured
//! mid-shield, or a RefreshAgent re-exec between freeze and thaw).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::Duration;

/// How long the workload stays frozen after the spawn. Cold start on a
/// quiet guest is 2.7 s (measured); 8 s covers spawn + init + the first
/// API send with margin, and bounds every failure mode below.
#[cfg(target_os = "linux")]
const SHIELD_GRACE: Duration = Duration::from_secs(8);

/// The currently-armed shield, so the startup backstop can tell "pids we
/// froze on purpose" from orphaned freezes.
static ACTIVE: Mutex<Option<Arc<ShieldState>>> = Mutex::new(None);

struct ShieldState {
    pids: Vec<i32>,
    thawed: AtomicBool,
}

impl ShieldState {
    fn thaw(&self, why: &str) {
        if self.thawed.swap(true, Ordering::SeqCst) {
            return; // timer and guard-drop race benignly
        }
        cont_pids(&self.pids);
        tracing::info!(pids = self.pids.len(), why, "storm shield thawed");
        let mut active = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cur) = active.as_ref() {
            if std::ptr::eq(cur.as_ref(), self) {
                *active = None;
            }
        }
    }
}

/// Held by the spawn path across the harness spawn. Dropping it thaws
/// immediately (the spawn failed — nothing to protect); [`Self::release`]
/// leaves the thaw to the grace timer (the spawn succeeded).
pub struct ShieldGuard {
    state: Arc<ShieldState>,
    released: bool,
}

impl ShieldGuard {
    /// The spawn succeeded: keep the workload frozen for the remainder
    /// of the grace window (the detached timer thaws it).
    pub fn release(mut self) {
        self.released = true;
    }
}

impl Drop for ShieldGuard {
    fn drop(&mut self) {
        if !self.released {
            self.state.thaw("spawn path unwound");
        }
    }
}

/// Startup backstop: a just-(re)started agentd owns no shield, so any
/// state-`T` process is an orphaned freeze — SIGCONT it. Cheap no-op on
/// a normal boot. Covers a freeze captured into a snapshot and a
/// RefreshAgent re-exec that dropped the thaw timer.
pub fn startup_audit() {
    audit_orphaned_freezes("agentd-startup");
}

/// Freeze the guest's userspace (everything but PID 1 and kernel
/// threads), arm the grace-timer thaw, and return the guard. `None`
/// when there was nothing to freeze or the platform has no /proc.
#[cfg(target_os = "linux")]
pub fn engage() -> Option<ShieldGuard> {
    let targets = freeze_targets();
    if targets.is_empty() {
        return None;
    }
    let stopped = stop_pids(&targets);
    if stopped.is_empty() {
        return None;
    }
    tracing::info!(
        frozen = stopped.len(),
        grace_secs = SHIELD_GRACE.as_secs(),
        "storm shield engaged: workload frozen for the harness cold start (ADR 0094)",
    );
    let state = Arc::new(ShieldState {
        pids: stopped,
        thawed: AtomicBool::new(false),
    });
    *ACTIVE.lock().unwrap_or_else(|p| p.into_inner()) = Some(state.clone());
    let timer_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(SHIELD_GRACE).await;
        timer_state.thaw("grace elapsed");
    });
    Some(ShieldGuard {
        state,
        released: false,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn engage() -> Option<ShieldGuard> {
    None
}

/// Every freezable pid right now: userspace processes except PID 1
/// (agentd — us) and kernel threads (empty cmdline). The harness is
/// spawned *after* the freeze, so it is never in this set.
#[cfg(target_os = "linux")]
fn freeze_targets() -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == 1 {
            continue;
        }
        // Kernel threads (and exiting processes) have an empty cmdline —
        // and a zombie's SIGSTOP would be a pointless no-op anyway.
        match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(cmdline) if !cmdline.is_empty() => pids.push(pid),
            _ => {}
        }
    }
    pids
}

/// SIGSTOP each pid, best-effort; returns the ones that took.
#[cfg(target_os = "linux")]
fn stop_pids(pids: &[i32]) -> Vec<i32> {
    let mut stopped = Vec::with_capacity(pids.len());
    for &pid in pids {
        if nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .is_ok()
        {
            stopped.push(pid);
        }
    }
    stopped
}

/// SIGCONT each pid, best-effort (ESRCH = it exited while frozen; fine).
fn cont_pids(pids: &[i32]) {
    #[cfg(target_os = "linux")]
    for &pid in pids {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGCONT,
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pids;
}

/// SIGCONT any state-`T` process the active shield doesn't own. Heals a
/// frozen set that outlived its shield (captured mid-shield; agentd
/// re-exec'd between freeze and timer). Deliberate in-guest SIGSTOPs are
/// sacrificed — acceptable in a single-workload guest, and each CONT is
/// logged.
#[cfg(target_os = "linux")]
fn audit_orphaned_freezes(trigger: &str) {
    let owned: Vec<i32> = ACTIVE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .filter(|s| !s.thawed.load(Ordering::SeqCst))
        .map(|s| s.pids.clone())
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == 1 || owned.contains(&pid) {
            continue;
        }
        if proc_state(pid) == Some('T') {
            tracing::warn!(
                pid,
                trigger,
                "orphaned frozen process (no active storm shield owns it); resuming",
            );
            cont_pids(&[pid]);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn audit_orphaned_freezes(_trigger: &str) {}

/// Process state letter from `/proc/<pid>/stat` — the field after the
/// parenthesized comm (which may itself contain spaces/parens, hence
/// `rsplit`).
#[cfg(target_os = "linux")]
fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.trim_start().chars().next()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Freeze/thaw round-trip on a real child. Unprivileged-safe: we own
    /// the child. Exercises stop_pids/cont_pids/proc_state — NOT the
    /// full engage() sweep, which would freeze the test host.
    #[test]
    fn stop_and_cont_roundtrip_on_own_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let stopped = stop_pids(&[pid]);
        assert_eq!(stopped, vec![pid]);
        // State flips to T (allow a beat for delivery).
        let mut state = None;
        for _ in 0..100 {
            state = proc_state(pid);
            if state == Some('T') {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state, Some('T'), "child must be stopped");
        cont_pids(&[pid]);
        let mut state = None;
        for _ in 0..100 {
            state = proc_state(pid);
            if state != Some('T') {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(state, Some('T'), "child must resume");
        let _ = child.kill();
        let _ = child.wait();
    }
}
