//! Clone-based snapshot manifest for `engram-sandbox-vz`.
//!
//! VZ's `saveMachineStateToURL` / `restoreMachineStateFromURL`
//! pair is broken upstream for arm64 Linux guests on macOS:
//! save succeeds but restore returns generic VZErrorRestore=12
//! "invalid argument" with no NSUnderlyingError. See UTM #6654,
//! Apple Developer Forum thread 745168, and the fact that
//! Apple's own `containerization` framework avoids the API
//! entirely for Linux. So we don't use it.
//!
//! Instead, snapshot semantics are clone-based:
//!   `snapshot` — pause VM → APFS-clone the per-sandbox rootfs
//!                into the snapshot dir → resume → write manifest.
//!                The clone IS the snapshot.
//!   `restore`  — read manifest → APFS-clone snapshot rootfs into
//!                a fresh per-sandbox file → cold-boot a fresh VM.
//!                Bootstrap supervisor + Claude `--resume <id>`
//!                handle conversation continuity across the
//!                cold boot.
//!
//! See `disk.rs` for the clone primitives.

use std::path::Path;

use chrono::{DateTime, Utc};
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};

use crate::disk::SNAPSHOT_ROOTFS_FILENAME;

/// Persisted next to the snapshot's rootfs clone so restore can
/// reconstruct the `VzVm` configuration (memory, cpu, etc.) exactly
/// as the source VM had it. The manifest's `spec.rootfs_source`
/// points at the snapshot's clone (not the original bake) — that's
/// what restore should attach to a fresh VM.
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

/// Filename of the JSON manifest under `<dest>/`.
pub(crate) const MANIFEST_FILENAME: &str = "manifest.json";

/// Build the `SnapshotMetadata` the trait's `snapshot()` returns.
/// `dest` already contains the rootfs clone and the manifest.
/// The reported size is dominated by the rootfs clone — APFS
/// clones report their nominal size (`stat`'s st_size) rather
/// than diverged-block usage, so this is a logical-size figure,
/// not actual disk consumption. Real on-disk usage is much
/// smaller until the sandbox writes diverging blocks.
pub(crate) async fn build_metadata(
    dest: &Path,
    image_version: &str,
    disk_manifest: Option<engram_core::types::manifest::ManifestRef>,
    snapshot_id: SnapshotId,
) -> Result<SnapshotMetadata, SandboxError> {
    let mut size_bytes = 0u64;
    for name in [SNAPSHOT_ROOTFS_FILENAME, MANIFEST_FILENAME] {
        let p = dest.join(name);
        size_bytes += tokio::fs::metadata(&p)
            .await
            .map_err(|e| {
                SandboxError::Snapshot(format!("stat snapshot artifact {}: {e}", p.display()))
            })?
            .len();
    }
    Ok(SnapshotMetadata {
        id: snapshot_id,
        size_bytes,
        created_at: Utc::now(),
        image_version: image_version.into(),
        disk_manifest,
        // VZ memory snapshots stay None — the Virtualization
        // framework's memory snapshot is broken upstream for arm64
        // guests (ADR 0003), so chunked memory is FC-only.
        memory_manifest: None,
        base_memory_manifest: None,
        // ADR 0014: VZ stays portable-snapshot-agnostic. The wrap
        // layer (PooledBackend) is FC-only on Linux; macOS dev paths
        // don't go through BlobStorage upload yet.
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: vec![],
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

    #[tokio::test]
    async fn build_metadata_carries_disk_manifest_when_provided() {
        // ADR 0007: the trait's `snapshot()` result threads the
        // chunked-disk manifest ref out for the coordinator to
        // persist on the `snapshots` row.
        let dir = tempfile::tempdir().unwrap();
        // Plant the artifacts `build_metadata` stats.
        tokio::fs::write(dir.path().join(SNAPSHOT_ROOTFS_FILENAME), b"fake-rootfs")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(MANIFEST_FILENAME), b"{}")
            .await
            .unwrap();

        let mref = engram_core::types::manifest::ManifestRef::new();
        let snap_id = SnapshotId::new();
        let meta = build_metadata(dir.path(), "img:1", Some(mref), snap_id)
            .await
            .unwrap();
        assert_eq!(meta.disk_manifest, Some(mref));
        assert_eq!(meta.image_version, "img:1");
        assert_eq!(meta.id, snap_id);

        // No chunk store wired → caller passes None → field stays None.
        let meta_no = build_metadata(dir.path(), "img:1", None, SnapshotId::new())
            .await
            .unwrap();
        assert_eq!(meta_no.disk_manifest, None);
    }
}
