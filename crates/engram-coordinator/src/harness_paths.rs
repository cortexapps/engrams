//! Guest-side harness path constants.
//!
//! Stage B2 made the coordinator stateless w.r.t. harness packs — they
//! live in the registry and the host-agent pulls them on first use into
//! its content-addressable cache, then assembles a per-session ext4
//! substrate that gets mounted into the VM at `/run/engram/harnesses`.
//!
//! The coordinator no longer scans a host directory or constructs host
//! paths. What it *does* still need are the in-VM identity bits — the
//! mount point and the well-known `argv[0]` shape — because the
//! `AgentSpec` it emits travels over the wire to the host-agent and
//! must reference the harness by its guest-visible path.

use std::path::Path;

const ENTRY_POINT_NAME: &str = "harness";

/// In-VM mount point. The host-agent mounts a per-session substrate
/// here read-only; `<mount>/<name>/harness` is what bootstrap exec's.
pub fn guest_mount_path() -> &'static Path {
    Path::new("/run/engram/harnesses")
}

/// Argv[0] for a session asking for harness `name`. The harness wrapper
/// resolves its sidecars relative to `argv[0]`, so the entire pack tree
/// must live under the same directory inside the VM.
pub fn guest_argv0(name: &str) -> String {
    format!(
        "{}/{}/{}",
        guest_mount_path().display(),
        name,
        ENTRY_POINT_NAME,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_argv0_points_inside_pack_directory() {
        assert_eq!(
            guest_argv0("claude"),
            "/run/engram/harnesses/claude/harness"
        );
    }
}
