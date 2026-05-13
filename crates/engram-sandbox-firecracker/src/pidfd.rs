//! ADR 0009 Phase 6: pidfd wrapper for live-VM reattach.
//!
//! `pidfd_open(2)` returns a file descriptor that references a
//! process by its PID. The fd is immune to PID recycling — once
//! opened, it always refers to the same process, even if the kernel
//! later reuses the integer pid for an unrelated process. That's
//! the property the Phase 6 reattach pass relies on: after a
//! host-agent restart we re-open `pidfd_open(recorded_pid)` and
//! the fd's identity ties us to the *original* FC process, not a
//! lookalike.
//!
//! The fd is `poll`-readable when the process exits (since Linux
//! 5.3). We wrap it in `tokio::io::unix::AsyncFd` so the
//! supervisor task can `.readable().await` and detect exit without
//! a polling loop.
//!
//! `waitid(P_PIDFD, fd, WNOHANG)` reaps the zombie (since 5.4). On
//! kernels older than 5.4 we'd need a different reaper; the FC
//! production target requires Linux 5.10+ anyway (the test kernel
//! in `~/.cache/engram-fc-test/vmlinux-5.10.223` is the minimum).
//!
//! Linux-only by construction. On macOS this module compiles
//! (cfg(target_os = "linux")-gated) but every function is a stub
//! returning `Err(PidFdError::Unsupported)` so the FC backend's
//! tests can still build on macOS-only contributors' laptops.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// Errors from pidfd operations. Mostly thin io::Error wrappers so
/// the caller can inspect raw_os_error() / kind() for ESRCH etc.
#[derive(Debug)]
pub enum PidFdError {
    /// Kernel rejected the pidfd_open: ESRCH (no such process),
    /// EINVAL (bad pid), ENOSYS (Linux < 5.3), etc.
    Open(io::Error),
    /// `waitid(P_PIDFD)` failed for some reason other than "process
    /// still running" (the latter returns `Ok(None)` from `try_wait`).
    Wait(io::Error),
    /// pidfd isn't supported on this platform. macOS, Windows, FreeBSD
    /// all fall here. Caller falls back to clean-slate.
    Unsupported,
}

impl std::fmt::Display for PidFdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(e) => write!(f, "pidfd_open: {e}"),
            Self::Wait(e) => write!(f, "waitid(P_PIDFD): {e}"),
            Self::Unsupported => write!(f, "pidfd not supported on this platform"),
        }
    }
}

impl std::error::Error for PidFdError {}

/// Owned pidfd handle. Drops the fd on Drop. `AsRawFd` so callers
/// can hand it to tokio's `AsyncFd`. Construction is via
/// [`open_pidfd`].
#[derive(Debug)]
pub struct PidFd {
    inner: OwnedFd,
    pid: u32,
}

impl PidFd {
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl AsRawFd for PidFd {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

/// Open a pidfd for `pid`. Linux 5.3+; returns
/// `Err(PidFdError::Unsupported)` on other platforms or kernels
/// that don't implement the syscall.
///
/// Returns `Err(PidFdError::Open(e))` where `e.kind() ==
/// io::ErrorKind::NotFound` when the pid doesn't exist (ESRCH). The
/// caller (live-attach) treats that as "FC process is already gone;
/// fall through to NVMe-restore or orphan-reap."
#[cfg(target_os = "linux")]
pub fn open_pidfd(pid: u32) -> Result<PidFd, PidFdError> {
    // SAFETY: SYS_pidfd_open takes a pid and flags. Passing
    // pid (the integer we recorded at FC spawn time, a u32 cast to
    // pid_t which is i32 in Linux) and flags=0 is documented in
    // `man 2 pidfd_open`. The syscall either returns a new fd
    // (caller takes ownership) or -1 with errno set. No kernel
    // memory is mutated by us; the syscall manages all state.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
    if raw < 0 {
        return Err(PidFdError::Open(io::Error::last_os_error()));
    }
    // SAFETY: pidfd_open returned a non-negative value, which the
    // syscall documents as a freshly-allocated fd we own. Wrap it
    // in OwnedFd so Drop closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
    Ok(PidFd { inner: fd, pid })
}

#[cfg(not(target_os = "linux"))]
pub fn open_pidfd(_pid: u32) -> Result<PidFd, PidFdError> {
    Err(PidFdError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn open_pidfd_for_self_succeeds() {
        let me = std::process::id();
        let fd = open_pidfd(me).expect("pidfd for self");
        assert_eq!(fd.pid(), me);
        // The raw fd should be a real positive integer.
        assert!(fd.as_raw_fd() >= 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_pidfd_for_dead_pid_returns_esrch() {
        // 16 million is well above the kernel's pid_max range
        // (default 4M); guaranteed-dead.
        let fake = 16_000_000_u32;
        let err = open_pidfd(fake).expect_err("must fail for nonexistent pid");
        match err {
            PidFdError::Open(io) => {
                assert_eq!(
                    io.raw_os_error(),
                    Some(libc::ESRCH),
                    "expected ESRCH, got {io:?}"
                );
            }
            other => panic!("wrong error variant: {other}"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn open_pidfd_is_unsupported_on_non_linux() {
        let me = std::process::id();
        let err = open_pidfd(me).expect_err("must be Unsupported on non-Linux");
        assert!(matches!(err, PidFdError::Unsupported));
    }
}
