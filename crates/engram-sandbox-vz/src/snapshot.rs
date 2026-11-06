//! Snapshot manifest for `engram-sandbox-vz`: a warm machine-state
//! restore when possible, a cold boot of the same disks otherwise.
//!
//! `snapshot` flushes the guest fs (unless the VM is already paused),
//! pauses, saves the machine state to `machine.vzs` (best effort: a
//! failure omits the warm block), resumes unless already paused, and
//! writes the manifest. The disks are the caller's; the caller captures
//! them under its own pause around this call.
//!
//! `restore` goes WARM when the gate passes (resume flavor; warm block
//! and `machine.vzs` present; saved cmdline equals today's; saved staged
//! bundles still exist; saved MAC not already live): it rebuilds the
//! byte-equivalent config, restores the machine state, attaches the
//! bridge while paused, resumes, and host-pushes the clock. Otherwise
//! it cold-boots the caller's disks.
//!
//! `machine.vzs` is protected by the user's keychain: same-user,
//! same-host restore only. It never leaves the host.

use std::path::Path;

use chrono::{DateTime, Utc};
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};

/// Persisted in the snapshot directory so restore can reconstruct the
/// `VzVm` configuration (memory, cpu, drives) exactly as the source VM
/// had it. The spec's disk paths name the source residence's disks;
/// restore attaches the disks its caller provides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VzSnapshotManifest {
    pub(crate) sandbox_id: SandboxId,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) spec: SandboxSpec,
    /// `"vz"`: a cross-VMM restore fails fast with a clear message.
    pub(crate) format: String,
    /// What a warm restore needs to rebuild the byte-equivalent
    /// configuration and resume `machine.vzs`. `None`: the save failed,
    /// cold boot only.
    #[serde(default)]
    pub(crate) warm: Option<WarmMachineState>,
}

/// The identity and effective config of the saved VM. One all-or-nothing
/// block: a partial identity is not warm-eligible.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WarmMachineState {
    /// The restoring VM's identifier and MAC must match the saved ones.
    pub(crate) machine_identifier: Vec<u8>,
    pub(crate) mac_address: String,
    /// The cmdline the saved VM booted with; restore gates on equality.
    pub(crate) kernel_cmdline: String,
    /// The aux drives attached to the saved VM, content shas pinned.
    pub(crate) aux_ro_drives: Vec<engram_core::types::sandbox::AuxRoDrive>,
    /// Resolved sizing, never recomputed from spec plus defaults.
    pub(crate) memory_mib: u32,
    pub(crate) vcpus: u32,
    /// The saved swap drive's size; the device shape is part of the
    /// equality contract. `None`: no swap drive.
    #[serde(default)]
    pub(crate) swap_mib: Option<u32>,
}

impl VzSnapshotManifest {
    pub(crate) fn new(
        sandbox_id: SandboxId,
        spec: SandboxSpec,
        warm: Option<WarmMachineState>,
    ) -> Self {
        Self {
            sandbox_id,
            created_at: Utc::now(),
            spec,
            format: "vz".into(),
            warm,
        }
    }
}

/// Filename of the JSON manifest under `<dest>/`.
pub(crate) const MANIFEST_FILENAME: &str = "manifest.json";

/// The saved machine state under `<dest>/`, present iff the manifest
/// carries a `warm` block. Keychain-protected: it never leaves the host.
pub(crate) const MACHINE_STATE_FILENAME: &str = "machine.vzs";

/// Build the `SnapshotMetadata` the trait's `snapshot()` returns.
/// `dest` already contains the manifest and, after a successful save,
/// the machine state. The disks are the caller's and are not counted.
pub(crate) async fn build_metadata(
    dest: &Path,
    image_version: &str,
    snapshot_id: SnapshotId,
) -> Result<SnapshotMetadata, SandboxError> {
    let manifest = dest.join(MANIFEST_FILENAME);
    let mut size_bytes = tokio::fs::metadata(&manifest)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!(
                "stat snapshot artifact {}: {e}",
                manifest.display()
            ))
        })?
        .len();
    if let Ok(md) = tokio::fs::metadata(dest.join(MACHINE_STATE_FILENAME)).await {
        size_bytes += md.len();
    }
    Ok(SnapshotMetadata {
        id: snapshot_id,
        size_bytes,
        created_at: Utc::now(),
        image_version: image_version.into(),
        // The disk owner publishes the disk manifests; the machine
        // state never leaves the host.
        disk_manifest: None,
        swap_manifest: None,
        memory_manifest: None,
        base_memory_manifest: None,
        base_disk_manifest: None,
        migration_source: None,
        // ADR 0014: VZ stays portable-snapshot-agnostic. The wrap
        // layer (PooledBackend) is FC-only on Linux; macOS dev paths
        // don't go through BlobStorage upload yet.
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: vec![],
        // Issue #529: VZ never routes through `PooledBackend::snapshot_begin`
        // (it gates on `supports_diff_checkpoints`, InvalidSpec → composed
        // path); the composed path's `now` fallback covers VZ same as today.
        paused_at: None,
        // ADR 0095: capture never stamps peer hints; the resume
        // assembler does, coordinator-side.
        peer_hints: Vec::new(),
    })
}

/// Read + parse the manifest at `<src>/manifest.json`.
pub(crate) async fn read_manifest(src: &Path) -> Result<VzSnapshotManifest, SandboxError> {
    let bytes = tokio::fs::read(src.join(MANIFEST_FILENAME))
        .await
        .map_err(|e| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn fake_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: Some(PathBuf::from("/tmp/rootfs.ext4")),
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 512 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
            swap_mib: None,
            swap_source: None,
            swap_manifest: None,
        }
    }

    #[test]
    fn manifest_without_warm_field_parses_cold() {
        let m = VzSnapshotManifest::new(SandboxId::new(), fake_spec(), None);
        let mut v: serde_json::Value = serde_json::to_value(&m).unwrap();
        v.as_object_mut().unwrap().remove("warm");
        let parsed: VzSnapshotManifest = serde_json::from_value(v).unwrap();
        assert!(
            parsed.warm.is_none(),
            "missing warm field must default to None"
        );
    }

    #[test]
    fn warm_block_round_trips() {
        let warm = WarmMachineState {
            machine_identifier: vec![1, 2, 3],
            mac_address: "0a:00:00:00:00:01".into(),
            kernel_cmdline: "console=hvc0 ENGRAM_EGRESS=1:2:3".into(),
            aux_ro_drives: vec![],
            memory_mib: 1024,
            vcpus: 2,
            swap_mib: Some(64),
        };
        let m = VzSnapshotManifest::new(SandboxId::new(), fake_spec(), Some(warm));
        let bytes = serde_json::to_vec(&m).unwrap();
        let parsed: VzSnapshotManifest = serde_json::from_slice(&bytes).unwrap();
        let w = parsed.warm.expect("warm survives");
        assert_eq!(w.machine_identifier, vec![1, 2, 3]);
        assert_eq!(w.mac_address, "0a:00:00:00:00:01");
        assert_eq!(w.memory_mib, 1024);
        assert_eq!(w.vcpus, 2);
        assert_eq!(w.swap_mib, Some(64));
    }

    #[tokio::test]
    async fn manifest_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = VzSnapshotManifest::new(SandboxId::new(), fake_spec(), None);
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
        let mut m = VzSnapshotManifest::new(SandboxId::new(), fake_spec(), None);
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

    #[tokio::test]
    async fn build_metadata_names_no_disk_manifest() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join(MANIFEST_FILENAME), b"{}")
            .await
            .unwrap();

        let snap_id = SnapshotId::new();
        let meta = build_metadata(dir.path(), "img:1", snap_id).await.unwrap();
        assert_eq!(meta.disk_manifest, None);
        assert_eq!(meta.image_version, "img:1");
        assert_eq!(meta.id, snap_id);
    }
}
