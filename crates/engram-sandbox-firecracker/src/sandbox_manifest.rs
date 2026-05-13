//! ADR 0009 Phase 5: on-disk per-sandbox manifest for FC.
//!
//! Written atomically at the end of `create()` (and updated on
//! snapshot/restore where the FC pid changes), deleted at the start
//! of `destroy()` after the sandbox teardown completes. Lives at
//! `<work_dir>/<sandbox_id>/sandbox.json` — inside the jail dir
//! that's wiped wholesale on clean destroy, so a clean shutdown
//! leaves no manifest behind. A host-agent crash mid-create or
//! mid-destroy can leave one; the Phase 6 startup reattach pass
//! tolerates these.
//!
//! Schema v1 — see `SandboxManifest` below. Phase 6 reads this to
//! decide whether to reattach via pidfd (path 1). Phase 8 fills
//! `last_local_snapshot` after a SIGTERM checkpoint so path 2 can
//! cold-restore from local NVMe when path 1 fails.
//!
//! Atomic write: `write_temp_then_rename`. Same primitive the
//! chunk-store uses; provides crash-safety against a partial write
//! being read as a valid manifest. Implementation lives here rather
//! than borrowed from chunk-store to keep the dependency arrow
//! pointing the same direction it already does.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use engram_core::types::sandbox::SandboxSpec;
use engram_core::SandboxId;
use serde::{Deserialize, Serialize};

/// Current schema version. Bumped on incompatible changes to the
/// on-disk JSON shape. The Phase 6 reattach pass refuses to read
/// older or newer versions — operator must manually evict stale
/// sandboxes before deploying a host-agent that bumped the version.
pub const SCHEMA_VERSION: u32 = 1;

/// Marker for the `backend` discriminator. VZ may eventually share
/// this format with a `"vz"` value and a different `firecracker`
/// vs `vz` payload struct.
pub const BACKEND_FIRECRACKER: &str = "firecracker";

/// Top-level on-disk manifest. The Phase 6 reattach pass deserializes
/// this; the Phase 8 SIGTERM-checkpoint pipeline updates
/// `last_local_snapshot` in place.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxManifest {
    pub schema_version: u32,
    pub sandbox_id: SandboxId,
    pub backend: String,
    pub spec: SandboxSpec,
    pub firecracker: FirecrackerProcessRecord,
    /// `None` when the sandbox was created with `net_pool=None`
    /// (test-mode FC). Phase 6 reattach skips network rehydration
    /// in that case.
    pub network: Option<NetworkRecord>,
    /// Present iff `RestoreMode::Uffd` was used and the handler is
    /// still alive. Same three-axis verification (pid + start_time +
    /// comm) on reattach.
    pub uffd_handler: Option<ProcessRecord>,
    /// Filled by Phase 8's SIGTERM checkpoint. `None` until then.
    /// Phase 6 path 2 (NVMe restore) reads this to know which local
    /// chunked manifests to restore from when path 1 fails.
    pub last_local_snapshot: Option<LocalSnapshotRef>,
}

/// Three-axis pid identity. Phase 6 reattach verifies all three
/// match the manifest before opening a `pidfd_open(pid)` and
/// reattaching — defends against the OS recycling the pid across a
/// host-agent restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22 — start time in jiffies since
    /// boot. The kernel never reuses a pid+starttime pair within
    /// a boot, so this is the strongest available recycling guard.
    pub start_time_jiffies: u64,
    /// `/proc/<pid>/comm` — the executable's basename. Sanity check
    /// against a recycled pid that happens to share a starttime
    /// (effectively impossible, but cheap to verify).
    pub comm: String,
}

/// FC-specific process record + the socket paths the reattach pass
/// needs to verify the API is still responsive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FirecrackerProcessRecord {
    #[serde(flatten)]
    pub process: ProcessRecord,
    /// Path to FC's HTTP-over-Unix-socket control API. Phase 6
    /// path 1 issues a `GET /` against this as the liveness check.
    pub api_socket: PathBuf,
    /// Base path of the vsock UDS (FC appends `_<port>` for each
    /// guest-initiated CONNECT). Recorded so reattach can confirm
    /// the kernel still has the bind, not just the FC process.
    pub vsock_uds_base: PathBuf,
    /// FC vsock CID for this sandbox. Coordinator-side `harness_dial`
    /// reads it to wire the guest's harness-back-dial; reattach
    /// rehydrates the per-sandbox `next_cid` allocator state.
    pub vsock_cid: u32,
}

/// Per-VM network state. Reattach uses this to mark the
/// `net_allocator` slot in-use (so future `create()` calls don't
/// hand out the same /30) and to verify the kernel still has the
/// TAP device and iptables chain. If any of those is missing, path
/// 1 fails and reattach falls through.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkRecord {
    pub tap_name: String,
    /// `.0` of the /30 — the network address.
    pub vm_cidr_network: std::net::Ipv4Addr,
    pub host_ip: std::net::Ipv4Addr,
    pub guest_ip: std::net::Ipv4Addr,
}

/// Phase 8: reference to a SIGTERM-time local-NVMe snapshot for
/// this sandbox. Path 2 of the reattach uses these manifest IDs
/// (plus `chunk_store.has_all_chunks`) to decide whether NVMe
/// restore is viable before spawning the restore pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalSnapshotRef {
    pub disk_manifest_id: uuid::Uuid,
    pub disk_manifest_version: u64,
    pub memory_manifest_id: Option<uuid::Uuid>,
    pub memory_manifest_version: Option<u64>,
    /// Wall-clock time the SIGTERM checkpoint completed. Diagnostic
    /// only — the reattach pass doesn't gate on age (a manifest is
    /// either restorable or it isn't; staleness shows up as a
    /// `has_all_chunks` miss).
    pub taken_at: chrono::DateTime<chrono::Utc>,
    /// Why the checkpoint was taken. Currently always `"sigterm"`;
    /// reserved for future "periodic", "operator", etc. triggers.
    pub trigger: String,
}

/// Canonical on-disk path. `<work_dir>/<sandbox_id>/sandbox.json` —
/// inside FC's jail dir so a clean destroy wipes the manifest along
/// with everything else.
pub fn manifest_path(work_dir: &Path, sandbox_id: SandboxId) -> PathBuf {
    work_dir.join(sandbox_id.to_string()).join("sandbox.json")
}

/// Read `/proc/<pid>/stat` and parse out the starttime field
/// (1-indexed field 22 in `man 5 proc`). Returns `None` if the
/// file doesn't exist or has an unexpected shape — caller treats
/// that as "process gone" / "unverifiable."
///
/// `comm` (`/proc/<pid>/comm`) is fetched separately by
/// [`read_proc_comm`]. We don't parse it out of `/proc/<pid>/stat`
/// because the comm field there is wrapped in parens that can
/// contain spaces, requiring fragile parsing.
pub fn read_proc_start_time_jiffies(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field is the second token, wrapped in parens. Skip
    // past the closing `)` to find the rest of the fields, then
    // split on whitespace. Field 22 (starttime) is the 20th
    // whitespace-separated token after the closing `)` (since
    // pid is the only field before, and comm is the second).
    let close_paren = raw.rfind(')')?;
    let rest = raw.get(close_paren + 1..)?.trim_start();
    // rest now starts with field 3. starttime is field 22, so
    // index 22 - 3 = 19 (zero-based).
    let starttime = rest.split_whitespace().nth(19)?;
    starttime.parse::<u64>().ok()
}

/// Read `/proc/<pid>/comm` — the executable's basename (truncated
/// to 15 bytes by the kernel). Returns `None` on missing file or
/// I/O error; caller treats as "process gone."
pub fn read_proc_comm(pid: u32) -> Option<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(raw.trim_end_matches('\n').to_string())
}

/// Errors from manifest read/write paths. Distinct from
/// `SandboxError` so the caller decides whether a write failure
/// should fail `create()` or just log + degrade to "no reattach
/// possible for this sandbox."
#[derive(Debug)]
pub enum ManifestError {
    Io(std::io::Error),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    SchemaVersion { found: u32, expected: u32 },
    BackendMismatch { found: String, expected: String },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sandbox manifest I/O: {e}"),
            Self::Serialize(e) => write!(f, "sandbox manifest serialize: {e}"),
            Self::Deserialize(e) => write!(f, "sandbox manifest deserialize: {e}"),
            Self::SchemaVersion { found, expected } => write!(
                f,
                "sandbox manifest schema_version {found}, this host-agent expects {expected}"
            ),
            Self::BackendMismatch { found, expected } => write!(
                f,
                "sandbox manifest backend `{found}`, this reader expects `{expected}`"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<std::io::Error> for ManifestError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Atomically write a manifest. Pattern: write to a sibling temp
/// file with a unique suffix, then `rename` into place. POSIX
/// `rename` on the same filesystem is atomic — readers see either
/// the old manifest or the new one, never a half-written file.
/// Matches the chunk-store's atomic-write pattern.
pub fn write_manifest(path: &Path, manifest: &SandboxManifest) -> Result<(), ManifestError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(manifest).map_err(ManifestError::Serialize)?;
    // Nonce against concurrent writes: unlikely for our single-writer
    // create()/snapshot()/destroy() pattern but cheap insurance.
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("partial-{nonce}.json"));
    std::fs::write(&tmp, &body)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Cleanup the partial on rename failure so we don't leak.
        let _ = std::fs::remove_file(&tmp);
        return Err(ManifestError::Io(e));
    }
    Ok(())
}

/// Read a manifest from disk. Validates `schema_version` and
/// `backend` discriminator before returning, so the Phase 6
/// reattach pass can trust the deserialized struct.
pub fn read_manifest(path: &Path) -> Result<SandboxManifest, ManifestError> {
    let body = std::fs::read(path)?;
    let m: SandboxManifest = serde_json::from_slice(&body).map_err(ManifestError::Deserialize)?;
    if m.schema_version != SCHEMA_VERSION {
        return Err(ManifestError::SchemaVersion {
            found: m.schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    if m.backend != BACKEND_FIRECRACKER {
        return Err(ManifestError::BackendMismatch {
            found: m.backend,
            expected: BACKEND_FIRECRACKER.to_string(),
        });
    }
    Ok(m)
}

/// Best-effort manifest delete. Used by `destroy()` AFTER sandbox
/// teardown completes; missing-file is fine (manifest was never
/// written, or already cleaned by a previous attempt).
pub fn delete_manifest(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(_) | Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};

    fn dummy_manifest() -> SandboxManifest {
        SandboxManifest {
            schema_version: SCHEMA_VERSION,
            sandbox_id: SandboxId::new(),
            backend: BACKEND_FIRECRACKER.to_string(),
            spec: SandboxSpec {
                image: "test:1".into(),
                rootfs_source: None,
                image_uri: None,
                harness_pack_uri: None,
                cpu: CpuLimit { vcpus: 1 },
                memory: MemoryLimit { max_mib: 64 },
                disk: DiskLimit { max_gib: 1 },
                ttl: None,
                env: Default::default(),
                workdir: None,
                harness_substrate: None,
                network: Default::default(),
                canonical_memory_manifest: None,
            },
            firecracker: FirecrackerProcessRecord {
                process: ProcessRecord {
                    pid: 12345,
                    start_time_jiffies: 4242,
                    comm: "firecracker".into(),
                },
                api_socket: PathBuf::from("/tmp/fc.sock"),
                vsock_uds_base: PathBuf::from("/tmp/sb.vsock"),
                vsock_cid: 3,
            },
            network: None,
            uffd_handler: None,
            last_local_snapshot: None,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sandbox.json");
        let m = dummy_manifest();
        write_manifest(&path, &m).expect("write");
        let back = read_manifest(&path).expect("read");
        assert_eq!(back.sandbox_id, m.sandbox_id);
        assert_eq!(back.firecracker.process.pid, 12345);
        assert_eq!(back.firecracker.process.start_time_jiffies, 4242);
        assert_eq!(back.firecracker.process.comm, "firecracker");
        assert_eq!(back.firecracker.vsock_cid, 3);
        assert!(back.network.is_none());
        assert!(back.uffd_handler.is_none());
        assert!(back.last_local_snapshot.is_none());
    }

    #[test]
    fn read_rejects_wrong_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sandbox.json");
        let mut m = dummy_manifest();
        m.schema_version = 999;
        write_manifest(&path, &m).expect("write");
        let err = read_manifest(&path).expect_err("must reject");
        match err {
            ManifestError::SchemaVersion { found: 999, .. } => {}
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn read_rejects_wrong_backend_discriminator() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sandbox.json");
        let mut m = dummy_manifest();
        m.backend = "vz".into();
        write_manifest(&path, &m).expect("write");
        let err = read_manifest(&path).expect_err("must reject");
        match err {
            ManifestError::BackendMismatch { found, .. } => assert_eq!(found, "vz"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn write_is_atomic_no_partial_file_on_success() {
        // After a successful write, no `.partial-*.json` files exist
        // alongside the target — they were renamed in place.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dir").join("sandbox.json");
        write_manifest(&path, &dummy_manifest()).unwrap();
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.contains("partial"))
            .collect();
        assert!(siblings.is_empty(), "no partial files: got {siblings:?}");
    }

    #[test]
    fn delete_manifest_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sandbox.json");
        // No file present.
        delete_manifest(&path);
        write_manifest(&path, &dummy_manifest()).unwrap();
        assert!(path.exists());
        delete_manifest(&path);
        assert!(!path.exists());
        delete_manifest(&path);
        assert!(!path.exists());
    }

    #[test]
    fn proc_helpers_return_none_for_dead_pid() {
        // Pick a pid that's astronomically unlikely to exist
        // (kernel default pid_max is 32768 or 4M; we go above both).
        let fake_pid = 16_000_000_u32;
        assert!(read_proc_start_time_jiffies(fake_pid).is_none());
        assert!(read_proc_comm(fake_pid).is_none());
    }

    /// Sanity check we can actually parse our own process's stat
    /// file. Only meaningful on Linux; mac/process don't expose
    /// /proc/<pid>/stat the same way.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_helpers_return_some_for_self() {
        let self_pid = std::process::id();
        let st = read_proc_start_time_jiffies(self_pid);
        let comm = read_proc_comm(self_pid);
        assert!(st.is_some(), "should be able to read own /proc/self/stat");
        assert!(comm.is_some(), "should be able to read own /proc/self/comm");
    }
}
