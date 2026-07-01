//! Unix-domain socket path helpers.
//!
//! `sockaddr_un.sun_path` is a fixed-size buffer — 104 bytes on macOS/BSD,
//! 108 on Linux — so a UDS *bind path* longer than that fails at bind with
//! `InvalidInput: "path must be shorter than SUN_LEN"`. Per-sandbox sockets
//! keyed by a 36-char UUID (plus a `.vsock_NNNN` suffix ≈ 47 bytes) overflow
//! the cap the moment the containing directory is even moderately deep — a
//! git-worktree checkout, a long `$HOME`, or (on macOS) `$TMPDIR` itself,
//! which resolves to `/var/folders/<hash>/T` (~50 bytes). Rooting these
//! sockets in a short, fixed directory keeps every bind path within the cap
//! regardless of how deep the caller's working directory is.
//!
//! Both sandbox backends that bind vsock UDS files share this: the VZ backend
//! (its per-port bridge sockets) and the Firecracker integration tests (a stub
//! API socket). Production Firecracker doesn't need it — the jailer already
//! roots its sockets in a short chroot — but it follows the same rule.

use std::path::PathBuf;

/// Longest UDS bind path the tightest supported platform (macOS) accepts.
/// Linux allows 108; we budget against the smaller of the two so a path that
/// fits here fits everywhere.
pub const SUN_PATH_MAX: usize = 104;

/// A short, process-local directory for binding per-sandbox unix-domain
/// sockets. Callers root their socket *files* here — rather than under a deep
/// `work_dir` — so bind paths stay within [`SUN_PATH_MAX`]. This is a pure
/// path policy (engram-core does no I/O): the caller is responsible for
/// `create_dir_all`ing the returned directory before binding into it.
///
/// `/tmp` is hardcoded deliberately: macOS's `$TMPDIR` (`/var/folders/…`) is
/// itself long enough to blow the cap, whereas `/tmp` (→ `/private/tmp`) is
/// short and writable on both macOS and Linux. The directory is keyed by pid
/// so concurrent processes never share sockets; files within it are expected
/// to carry their own unique identity (a sandbox UUID), so a single process's
/// callers coexist without collision. Socket files are unlinked by their
/// owners; the (empty) directory is left behind, like any `/tmp` scratch.
pub fn short_socket_dir() -> PathBuf {
    PathBuf::from("/tmp").join(format!("engram-sock-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_socket_dir_keeps_bind_paths_within_sun_len() {
        // The pathological shape this exists to defend: a 36-char UUID plus
        // the longest per-port suffix the VZ bridge appends. Under a deep
        // work_dir this would overflow; rooted in the short dir it must not.
        let file = "00000000-0000-0000-0000-000000000000.vsock_1030";
        let path = short_socket_dir().join(file);
        let len = path.as_os_str().len();
        assert!(
            len <= SUN_PATH_MAX,
            "bind path {} is {len}B, over the {SUN_PATH_MAX}B SUN_LEN cap",
            path.display(),
        );
    }
}
