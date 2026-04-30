//! Snapshot / restore via VZ's `saveMachineStateToURL` /
//! `restoreMachineStateFromURL` (macOS 14+).
//!
//! State-machine contract (per Apple's docs):
//!   `save`    — VM must be `.paused`. Output is a single file
//!               carrying memory + device state.
//!   `restore` — VM must be `.stopped`. After restore, VM is in
//!               `.paused`; caller must `resume()` to bring it
//!               online.
//!
//! `VzBackend::snapshot` orchestrates pause → save → resume →
//! manifest.json. `VzBackend::restore` reads the manifest, rebuilds
//! a `VzVm` from the captured spec, and calls `vm.restore(state.bin)`
//! followed by `vm.resume()`. Returns a fresh `SandboxId` —
//! mirrors the FC contract.
//!
//! No UFFD-equivalent on VZ — `restoreMachineStateFromURL` reads
//! the full state file synchronously. That's a perf trade-off, not
//! a correctness one.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};

/// Persisted next to `state.bin` so restore can reconstruct the
/// `VzVm` configuration (kernel + rootfs + memory + cpu) exactly as
/// the source VM had it. Without this, restoring a save file built
/// against a different rootfs version would be a silent
/// mis-configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VzSnapshotManifest {
    pub(crate) sandbox_id: SandboxId,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) spec: SandboxSpec,
    /// Tag carried for human readers + future migration logic.
    /// `"vz"` distinguishes from the FC-flavored manifests so a
    /// cross-VMM restore attempt can fail-fast with a clear message.
    pub(crate) format: String,
}

impl VzSnapshotManifest {
    pub(crate) fn new(sandbox_id: SandboxId, spec: SandboxSpec) -> Self {
        Self {
            sandbox_id,
            created_at: Utc::now(),
            spec,
            format: "vz".into(),
        }
    }
}

/// Filename of the per-snapshot VZ state file under `<dest>/`.
pub(crate) const STATE_FILENAME: &str = "state.bin";
/// Filename of the JSON manifest under `<dest>/`.
pub(crate) const MANIFEST_FILENAME: &str = "manifest.json";

/// Build the `SnapshotMetadata` the trait's `snapshot()` returns.
/// `dest` already contains state.bin and manifest.json.
pub(crate) async fn build_metadata(
    dest: &Path,
    image_version: &str,
) -> Result<SnapshotMetadata, SandboxError> {
    let mut size_bytes = 0u64;
    for name in [STATE_FILENAME, MANIFEST_FILENAME] {
        let p = dest.join(name);
        size_bytes += tokio::fs::metadata(&p)
            .await
            .map_err(|e| {
                SandboxError::Snapshot(format!(
                    "stat snapshot artifact {}: {e}",
                    p.display()
                ))
            })?
            .len();
    }
    Ok(SnapshotMetadata {
        id: SnapshotId::new(),
        size_bytes,
        created_at: Utc::now(),
        image_version: image_version.into(),
    })
}

/// Read + parse the manifest at `<src>/manifest.json`.
pub(crate) async fn read_manifest(src: &Path) -> Result<VzSnapshotManifest, SandboxError> {
    let bytes = tokio::fs::read(src.join(MANIFEST_FILENAME)).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "read snapshot manifest {}/{MANIFEST_FILENAME}: {e}",
            src.display()
        ))
    })?;
    let manifest: VzSnapshotManifest = serde_json::from_slice(&bytes)
        .map_err(|e| SandboxError::Snapshot(format!("parse manifest: {e}")))?;
    if manifest.format != "vz" {
        return Err(SandboxError::Snapshot(format!(
            "manifest format {:?} is not 'vz' — cross-VMM restore not supported",
            manifest.format
        )));
    }
    Ok(manifest)
}

/// Resolve the path to the VZ state file inside `<src>/`.
pub(crate) fn state_path(src: &Path) -> PathBuf {
    src.join(STATE_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use std::collections::HashMap;

    fn fake_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: Some(PathBuf::from("/tmp/rootfs.ext4")),
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 512 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
        }
    }

    #[tokio::test]
    async fn manifest_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = VzSnapshotManifest::new(SandboxId::new(), fake_spec());
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        tokio::fs::write(dir.path().join(MANIFEST_FILENAME), &bytes)
            .await
            .unwrap();
        let parsed = read_manifest(dir.path()).await.unwrap();
        assert_eq!(parsed.sandbox_id, manifest.sandbox_id);
        assert_eq!(parsed.format, "vz");
    }

    #[tokio::test]
    async fn read_manifest_rejects_non_vz_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = VzSnapshotManifest::new(SandboxId::new(), fake_spec());
        m.format = "fc".into();
        tokio::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            serde_json::to_vec(&m).unwrap(),
        )
        .await
        .unwrap();
        let err = read_manifest(dir.path()).await.unwrap_err();
        assert!(err.to_string().contains("not 'vz'"));
    }
}
