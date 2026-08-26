//! The daemon's on-disk manifest: `<work_dir>/egress-proxyd.json`.
//!
//! Written by proxyd ITSELF, atomically, only after its listeners are
//! bound — durable at the point the fact becomes true (the PR #1383
//! blueprint). A manifest therefore always describes a daemon that
//! once served; whether it still does is decided against the live
//! process (three-axis identity) and the live socket (`Hello` + the
//! accept-loop probe), never against this file alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// What the daemon records about itself for the successor host-agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProxydManifest {
    pub schema_version: u32,
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22 — start time in jiffies since
    /// boot. The kernel never reuses a pid+starttime pair within a
    /// boot (the `sandbox_manifest::ProcessRecord` guard).
    pub start_time_jiffies: u64,
    /// `/proc/<pid>/comm` — the executable's basename.
    pub comm: String,
    /// The build-time source fingerprint, `None` for local builds.
    pub source_fingerprint: Option<String>,
    pub proxy_port: u16,
    pub dns_port: u16,
    pub gateway_port: u16,
    pub control_sock: PathBuf,
}

/// Path of the manifest inside `work_dir`.
pub fn manifest_path(work_dir: &Path) -> PathBuf {
    work_dir.join(crate::MANIFEST_NAME)
}

/// Atomic write (born-in-tmp + rename), the `bindings.rs` shape. The
/// manifest carries no secrets, so no mode tightening is needed —
/// the work dir is root-owned anyway.
pub fn write_manifest(work_dir: &Path, m: &ProxydManifest) -> std::io::Result<()> {
    let final_path = manifest_path(work_dir);
    let tmp = work_dir.join(format!(
        ".{}.{}.tmp",
        crate::MANIFEST_NAME,
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp, &final_path)
}

/// Read the manifest. `Ok(None)` = no daemon has ever served here (or
/// the file is unreadable garbage — the caller spawns fresh either
/// way, and the fresh daemon's write replaces the garbage).
pub fn read_manifest(work_dir: &Path) -> Option<ProxydManifest> {
    let bytes = std::fs::read(manifest_path(work_dir)).ok()?;
    let m: ProxydManifest = serde_json::from_slice(&bytes)
        .inspect_err(|e| {
            tracing_unusable_manifest(&manifest_path(work_dir), &e.to_string());
        })
        .ok()?;
    if m.schema_version != MANIFEST_SCHEMA_VERSION {
        tracing_unusable_manifest(
            &manifest_path(work_dir),
            &format!(
                "schema {} (want {MANIFEST_SCHEMA_VERSION})",
                m.schema_version
            ),
        );
        return None;
    }
    Some(m)
}

// This crate has no tracing dep on purpose (protocol crates stay
// lean); an unusable manifest is surfaced on stderr, which both the
// host-agent's pod log and the daemon's log file capture.
fn tracing_unusable_manifest(path: &Path, why: &str) {
    eprintln!(
        "egress-proxyd manifest {} unusable ({why}); treating as absent",
        path.display()
    );
}

/// Remove the manifest (best-effort). Called after a stale daemon is
/// killed so a crash before the fresh spawn leaves no lying record.
pub fn remove_manifest(work_dir: &Path) {
    let _ = std::fs::remove_file(manifest_path(work_dir));
}

/// Three-axis live identity of a process, as read from `/proc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcIdentity {
    pub pid: u32,
    pub start_time_jiffies: u64,
    pub comm: String,
}

impl ProcIdentity {
    /// Does this live process match the manifest's recorded identity?
    pub fn matches(&self, m: &ProxydManifest) -> bool {
        self.pid == m.pid && self.start_time_jiffies == m.start_time_jiffies && self.comm == m.comm
    }
}

/// Read the live identity of `pid` from `/proc`. `None` = process
/// gone, unreadable, or a zombie (a zombie can never serve again and
/// only its parent can reap it — the issue #1012 lesson). Linux only;
/// other platforms always return `None` and the caller falls back to
/// socket-liveness (dev spawns fresh).
#[cfg(target_os = "linux")]
pub fn read_proc_identity(pid: u32) -> Option<ProcIdentity> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is paren-wrapped and may contain spaces; parse
    // from the last `)` (the `sandbox_manifest` readers' approach).
    let close_paren = raw.rfind(')')?;
    let rest = raw.get(close_paren + 1..)?.trim_start();
    // rest starts at field 3 (state). starttime is field 22 →
    // zero-based index 19.
    let state = rest.chars().next()?;
    if state == 'Z' {
        return None;
    }
    let start_time_jiffies = rest.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()?
        .trim_end_matches('\n')
        .to_string();
    Some(ProcIdentity {
        pid,
        start_time_jiffies,
        comm,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_identity(_pid: u32) -> Option<ProcIdentity> {
    None
}

/// The daemon's own identity, for stamping into the manifest it
/// writes. On non-Linux the jiffies axis is 0 and `matches` never
/// runs (adopt falls back to socket-liveness there).
pub fn self_identity() -> ProcIdentity {
    let pid = std::process::id();
    #[cfg(target_os = "linux")]
    if let Some(id) = read_proc_identity(pid) {
        return id;
    }
    ProcIdentity {
        pid,
        start_time_jiffies: 0,
        comm: current_comm(),
    }
}

fn current_comm() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        // The kernel truncates comm to 15 bytes; mirror it so a
        // manifest written here compares equal to a /proc read.
        .map(|mut s| {
            s.truncate(15);
            s
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> ProxydManifest {
        ProxydManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            pid: 4242,
            start_time_jiffies: 123_456,
            comm: "engram-egress-p".into(),
            source_fingerprint: Some("f00d".into()),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            control_sock: dir.join(crate::CONTROL_SOCK_NAME),
        }
    }

    #[test]
    fn manifest_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample(dir.path());
        write_manifest(dir.path(), &m).unwrap();
        assert_eq!(read_manifest(dir.path()), Some(m));
    }

    #[test]
    fn missing_and_garbage_manifests_read_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_manifest(dir.path()), None);
        std::fs::write(manifest_path(dir.path()), b"{not json").unwrap();
        assert_eq!(read_manifest(dir.path()), None);
    }

    #[test]
    fn unknown_schema_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = sample(dir.path());
        m.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        write_manifest(dir.path(), &m).unwrap();
        assert_eq!(read_manifest(dir.path()), None);
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        remove_manifest(dir.path());
        write_manifest(dir.path(), &sample(dir.path())).unwrap();
        remove_manifest(dir.path());
        assert_eq!(read_manifest(dir.path()), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn self_identity_reads_own_proc() {
        let id = read_proc_identity(std::process::id()).expect("own /proc readable");
        assert_eq!(id.pid, std::process::id());
        assert!(id.start_time_jiffies > 0);
        assert!(!id.comm.is_empty());
    }
}
