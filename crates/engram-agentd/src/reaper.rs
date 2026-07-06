//! Init-style zombie reaping (issue #569).
//!
//! agentd is PID 1 inside the guest. The in-guest browser stack (Xvfb →
//! openbox → chromium → x11vnc, [`crate::browser`]) is deliberately spawned
//! DETACHED (`setsid`) so it survives whichever `StartBrowser` connection
//! triggered it; once its short-lived launcher exits, the whole stack
//! reparents to pid 1 per POSIX orphan-reparenting. Nothing was ever waiting
//! on those pids, so on exit they become zombies — and with no init-style
//! reaper, they piled up without bound (#569 observed hundreds of zombie
//! Xvfb/openbox/chromium processes in a long-lived guest).
//!
//! **The hazard this module is careful about.** agentd ALSO holds several
//! `tokio::process::Child` handles it fully owns and eventually
//! `.wait()`s/`.try_wait()`s itself: the `/exec` child ([`crate::handler`]),
//! the harness supervisor's cached child ([`crate::harness_supervisor`] —
//! whose `try_wait()` result drives the live-vs-exited reattach decision on
//! live teleport, load-bearing), and ttyd ([`crate::shell`]). tokio's own
//! reaper only background-reaps children it is actively polling; a
//! held-but-not-`.await`ed `Child` (exactly the harness supervisor's
//! between-`SpawnHarness`-calls state) is NOT background-reaped. If this
//! module's `/proc` scan indiscriminately reaped every zombie child of
//! `process::id()`, it would steal that exit status out from under the
//! supervisor and the next `try_wait()` would see `ECHILD` instead of the
//! real status. So: every tokio-tracked child is registered here at spawn
//! (via [`track`]/[`TrackedChild`]) and deregistered once its owner has
//! consumed its exit status; the reaper skips anything still registered.
//!
//! **Never `waitpid(-1, ...)`.** That call harvests ANY child, tracked or
//! not, racing tokio's own reaper for the same pids. We only ever
//! `waitpid(Pid::from_raw(pid), WNOHANG)` on a SPECIFIC pid we've already
//! confirmed via `/proc/<pid>/stat` is (a) in state `Z` and (b) a direct
//! child of this process and (c) not in the tracked registry.
//!
//! Linux-only real implementation (the guest is always Linux); a no-op stub
//! elsewhere, mirroring the pattern in [`crate::browser::terminate_pgid`].

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Process-wide registry of pids owned by a live `tokio::process::Child`
/// handle elsewhere in agentd. The reaper's scan skips anything in here —
/// see the module-level hazard note.
fn tracked() -> &'static Mutex<HashSet<u32>> {
    static TRACKED: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    TRACKED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Register `pid` as owned by a tokio-tracked `Child` handle — the reaper
/// must never touch it. Idempotent.
pub fn track(pid: u32) {
    tracked()
        .lock()
        .expect("tracked-pid registry poisoned")
        .insert(pid);
}

/// Deregister `pid` once its owner has consumed the final exit status (or
/// given the child up entirely). Idempotent — untracking a pid that was
/// never tracked (or already untracked) is a no-op.
pub fn untrack(pid: u32) {
    tracked()
        .lock()
        .expect("tracked-pid registry poisoned")
        .remove(&pid);
}

/// Spawn `cmd` and register its pid with the tracked registry *before*
/// releasing the registry lock — the atomic sibling of `spawn` +
/// [`track`], which have a window between them where the reaper's `/proc`
/// scan could see the freshly-spawned pid as a `Z` (a child that execve'd
/// straight into exit, or lost its own race against the scheduler) and
/// steal its exit status before the caller ever calls `track`. Every
/// tokio-tracked spawn site in this crate goes through this function
/// instead of `Command::spawn` directly.
///
/// `Command::spawn` is synchronous (it's the surrounding `Child` that's
/// async), so holding the registry mutex across spawn+insert costs one
/// uncontended lock/unlock, not a fork/exec under lock.
#[cfg(target_os = "linux")]
pub fn spawn_tracked(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    let mut guard = tracked().lock().expect("tracked-pid registry poisoned");
    let child = cmd.spawn()?;
    if let Some(pid) = child.id() {
        guard.insert(pid);
    }
    Ok(child)
}

/// Non-Linux stub (see the module doc): no reaper, no registry, just spawn.
#[cfg(not(target_os = "linux"))]
pub fn spawn_tracked(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    cmd.spawn()
}

/// RAII sibling of [`track`]/[`untrack`] for the common case: a `Child`
/// handle whose owner holds it for one bounded scope (spawn → wait, no
/// intermediate state transitions to reason about — e.g. the `/exec`
/// child). Hold this alongside the `Child`; it untracks on drop regardless
/// of which return path the scope takes.
///
/// The harness supervisor's cached child does NOT use this: its lifecycle
/// (spawn / live-reattach / reap-and-respawn) has meaningfully different
/// track/untrack points that an RAII guard tied to one `Child` value can't
/// express, so it calls [`track`]/[`untrack`] directly at each transition.
pub struct TrackedChild(u32);

impl TrackedChild {
    pub fn new(pid: u32) -> Self {
        track(pid);
        Self(pid)
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        untrack(self.0);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::tracked;
    use std::collections::HashSet;
    use std::time::Duration;

    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::Pid;
    use tokio::signal::unix::{signal, SignalKind};

    /// Fallback wake interval when no SIGCHLD arrives — catches any zombie
    /// whose signal we raced (e.g. one that landed before the handler was
    /// installed) or whose SIGCHLD coalesced with another's under load
    /// (POSIX doesn't queue multiple pending instances of the same signal).
    const FALLBACK_TICK: Duration = Duration::from_secs(10);

    /// One `/proc/<pid>/stat` record, just the fields the scan needs.
    struct ProcStat {
        state: char,
        ppid: u32,
    }

    /// Parse the state + ppid fields out of `/proc/<pid>/stat` contents.
    /// `comm` (field 2, parenthesized) can itself contain spaces or
    /// parentheses, so the state field is whatever follows the LAST `)` —
    /// never a fixed whitespace-split index.
    fn parse_stat(contents: &str) -> Option<ProcStat> {
        let after_comm = contents.rsplit(')').next()?;
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let ppid: u32 = fields.next()?.parse().ok()?;
        Some(ProcStat { state, ppid })
    }

    /// Every pid in `/proc` that is a zombie (`state == 'Z'`) and a direct
    /// child (`ppid == self_pid`) of this process. Best-effort: a pid
    /// disappearing mid-scan (exited-and-reaped by its real owner between
    /// the readdir and the stat read) is silently skipped, not an error.
    fn zombie_children_of(self_pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue; // not a pid dir (e.g. /proc/self, /proc/cpuinfo)
            };
            let Ok(contents) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if let Some(stat) = parse_stat(&contents) {
                if stat.state == 'Z' && stat.ppid == self_pid {
                    out.push(pid);
                }
            }
        }
        out
    }

    /// Reap every zombie child of `self_pid` not present in `tracked_pids`.
    /// Returns the pids actually reaped, for logging (production) or
    /// assertions (tests). Split out of the signal/tick loop so tests can
    /// drive it directly against an explicit self-pid + registry snapshot,
    /// without waiting on a real SIGCHLD/timer.
    ///
    /// `waitpid(pid, WNOHANG)` on a confirmed state-`Z` pid never blocks.
    /// `ECHILD` (another waiter already reaped it — a benign race with, in
    /// theory, a concurrent tick) and any other error are swallowed: a
    /// missed zombie just gets picked up on the next tick.
    fn reap_scan(self_pid: u32, tracked_pids: &HashSet<u32>) -> Vec<u32> {
        let mut reaped = Vec::new();
        for pid in zombie_children_of(self_pid) {
            if tracked_pids.contains(&pid) {
                continue;
            }
            match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
                Ok(status) => {
                    tracing::debug!(pid, ?status, "reaper: reaped orphaned zombie");
                    reaped.push(pid);
                }
                Err(nix::errno::Errno::ECHILD) => {
                    // Raced another reap of this exact pid (or it was never
                    // really our child) — not an error, just a no-op.
                }
                Err(e) => {
                    tracing::debug!(pid, error = %e, "reaper: waitpid failed");
                }
            }
        }
        reaped
    }

    /// Hold the SAME registry lock across the whole `/proc` scan + `waitpid`
    /// loop — no pre-scan clone-and-release. Invariant this buys us: any
    /// child whose `Z` state this scan observes was spawned (and, via
    /// [`super::spawn_tracked`]'s own atomicity, already registered if it's
    /// tokio-tracked) strictly before the scan acquired the lock — a
    /// `spawn_tracked` racing the scan either finishes registering before
    /// the scan starts (and is correctly skipped) or blocks on the lock
    /// until after the scan finishes (and its child, being freshly spawned,
    /// can't be a zombie yet). Either way the reaper can never observe a
    /// tokio-tracked pid as untracked. The scan itself is ms-scale (a
    /// `/proc` walk over a small guest's pid space) and `spawn_tracked` is
    /// called rarely (once per StartShell/StartBrowser/exec/harness-spawn
    /// RPC), so contending this lock for the scan's duration is cheap.
    fn reap_tick() {
        let self_pid = std::process::id();
        let guard = tracked().lock().expect("tracked-pid registry poisoned");
        // Per-pid reap outcomes are logged inside `reap_scan` (carries the
        // exit status); no separate aggregate log here.
        let _reaped = reap_scan(self_pid, &guard);
    }

    /// The reaper's whole lifetime loop: wake on SIGCHLD or the periodic
    /// fallback tick, whichever comes first, and scan on every wake. Runs
    /// forever — spawned once from `main.rs::run()` and never joined.
    async fn run() {
        let mut sigchld = match signal(SignalKind::child()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reaper: SIGCHLD handler install failed; falling back to the periodic tick only",
                );
                None
            }
        };
        loop {
            match sigchld.as_mut() {
                Some(sig) => {
                    tokio::select! {
                        _ = sig.recv() => {}
                        _ = tokio::time::sleep(FALLBACK_TICK) => {}
                    }
                }
                None => tokio::time::sleep(FALLBACK_TICK).await,
            }
            reap_tick();
        }
    }

    pub fn spawn() {
        tokio::spawn(run());
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Instant;

        /// Spawn a child that exits almost immediately and return the handle
        /// WITHOUT waiting — from the kernel's perspective it becomes a
        /// zombie parented to this test process (`std::process::id()`),
        /// exactly the orphaned-browser-process shape #569 hit (modulo the
        /// reparent hop, which doesn't change the ppid-match logic under
        /// test). Every test eventually calls `.wait()` on the handle — as
        /// an assertion (it errors once the reaper stole the status) or as
        /// the owner's own reap — so no zombie outlives the test.
        fn spawn_zombie() -> std::process::Child {
            std::process::Command::new("true")
                .spawn()
                .expect("spawn `true`")
        }

        /// Poll `/proc/<pid>/stat` until the kernel reports state `Z`
        /// (or the pid vanished, which would mean something else already
        /// reaped it — the test should fail loudly rather than hang).
        fn wait_for_zombie(pid: u32, deadline: Instant) -> bool {
            loop {
                match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Ok(contents) => {
                        if parse_stat(&contents).map(|s| s.state) == Some('Z') {
                            return true;
                        }
                    }
                    Err(_) => return false, // gone already
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn untracked_orphaned_zombie_is_reaped() {
            let mut child = spawn_zombie();
            let pid = child.id();
            let deadline = Instant::now() + Duration::from_secs(2);
            assert!(
                wait_for_zombie(pid, deadline),
                "child {pid} never reached zombie state"
            );

            let self_pid = std::process::id();
            let reaped = reap_scan(self_pid, &HashSet::new());
            assert!(
                reaped.contains(&pid),
                "reap_scan should have collected untracked zombie {pid}; got {reaped:?}"
            );

            // Confirmed gone: the handle's own wait must now fail (ECHILD —
            // the reaper already collected the status). This is also the
            // cleanup path: had reap_scan NOT collected it, this wait would.
            assert!(
                child.wait().is_err(),
                "pid {pid} should already be reaped; the owner's wait must fail"
            );
        }

        #[test]
        fn tracked_zombie_is_skipped_and_owner_still_gets_the_status() {
            let mut child = spawn_zombie();
            let pid = child.id();
            let deadline = Instant::now() + Duration::from_secs(2);
            assert!(
                wait_for_zombie(pid, deadline),
                "child {pid} never reached zombie state"
            );

            let self_pid = std::process::id();
            let mut tracked_pids = HashSet::new();
            tracked_pids.insert(pid);

            let reaped = reap_scan(self_pid, &tracked_pids);
            assert!(
                !reaped.contains(&pid),
                "reap_scan must not touch a tracked pid; reaped {reaped:?}"
            );

            // The "owner" (this test, standing in for e.g. the harness
            // supervisor) can still retrieve the real exit status — proving
            // the reaper didn't steal it.
            let status = child
                .wait()
                .expect("owner's own wait should still see the zombie");
            assert!(
                status.success(),
                "expected `true`'s clean exit via the owner's wait, got {status:?}"
            );
        }

        /// The atomicity property [`super::super::spawn_tracked`] exists for:
        /// a pid it spawns must be registered by the time it returns, so a
        /// concurrent `reap_scan` can never observe it as an untracked
        /// zombie — even one that exits before the caller gets around to its
        /// own `wait()`. This spawns a child that exits almost immediately
        /// via `spawn_tracked`, waits for the kernel to report it as a
        /// zombie, then runs `reap_scan` against the CURRENT registry (no
        /// snapshot-then-race window, matching how `reap_tick` now holds the
        /// lock) and asserts the pid was already shielded.
        #[tokio::test]
        async fn spawn_tracked_registers_before_a_concurrent_scan_can_see_it() {
            let mut cmd = tokio::process::Command::new("true");
            let mut child = super::super::spawn_tracked(&mut cmd).expect("spawn_tracked `true`");
            let pid = child.id().expect("freshly spawned child has a pid");

            let deadline = Instant::now() + Duration::from_secs(2);
            assert!(
                wait_for_zombie(pid, deadline),
                "child {pid} never reached zombie state"
            );

            let self_pid = std::process::id();
            let snapshot = tracked()
                .lock()
                .expect("tracked-pid registry poisoned")
                .clone();
            let reaped = reap_scan(self_pid, &snapshot);
            assert!(
                !reaped.contains(&pid),
                "spawn_tracked's pid must already be registered by the time a scan can \
                 see its zombie; reaped {reaped:?}"
            );

            // Cleanup: this test is the "owner" — untrack and reap ourselves
            // so we don't leak a zombie into the rest of the test binary.
            super::super::untrack(pid);
            child.wait().await.expect("owner's own wait should succeed");
        }
    }
}

#[cfg(target_os = "linux")]
pub fn spawn() {
    linux::spawn();
}

/// No-op stub: the guest is always Linux, so this only matters for the
/// macOS cross-build of the crate (dev-host cargo check/clippy) where
/// there's no `/proc` and no orphaned in-guest stack to reap.
#[cfg(not(target_os = "linux"))]
pub fn spawn() {}
