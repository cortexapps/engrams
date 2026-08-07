//! Snapshot manifest for `engram-sandbox-vz` — warm machine-state
//! restore when possible, clone+cold-boot always available (ADR 0096
//! D7 productized what the spike proved).
//!
//! History: VZ originally used clone+cold-boot ONLY, because
//! `restoreMachineStateFromURL` returned an opaque VZErrorRestore=12
//! for arm64 Linux guests (ADR 0003 era; UTM #6654, Apple forum
//! 745168). Re-validated 2026-07-15 on macOS 26: the failure was a
//! CONFIG-IDENTITY mismatch, not a Linux-guest limitation — restore
//! works when both the `VZGenericMachineIdentifier` AND the
//! virtio-net MAC are pinned across the boundary (the framework mints
//! a fresh random MAC per configuration otherwise). The standing
//! probes are `vm::tests::machine_state_save_restore_spike` and the
//! round-3 siblings.
//!
//! Snapshot semantics:
//!   `snapshot` — flush guest fs (unless parked) → pause → APFS-clone
//!                the rootfs → BEST-EFFORT `saveMachineStateToURL` to
//!                `machine.vzs` (~working-set-sized; a failure just
//!                omits the warm block) → resume-unless-parked →
//!                manifest (with [`WarmMachineState`] iff saved).
//!   `restore`  — WARM when the gate passes (resume flavor only; warm
//!                block + machine.vzs present; saved cmdline equals
//!                today's; saved staged bundles still exist; saved MAC
//!                not already live): rebuild the byte-equivalent
//!                config, `restoreMachineStateFromURL`, attach the
//!                bridge while paused, resume, host-push the clock
//!                (StepClock). Memory intact — no cold boot, no
//!                Claude `--resume` crutch.
//!                COLD otherwise (and on ANY warm failure, after
//!                re-cloning the rootfs): fresh per-sandbox clone,
//!                re-resolve the agentd slot, cold-boot — exactly the
//!                pre-D7 behavior.
//!
//! Constraints (by Apple's design): `machine.vzs` is protected via
//! the user's keychain — SAME-USER, SAME-HOST restore only (headless
//! runners with a locked login keychain simply never produce warm
//! blocks); it is NEVER chunked to BlobStorage, and cross-host
//! restore stays cold-boot via disk chunks.
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
    /// ADR 0096 D7: everything a WARM restore needs to rebuild the
    /// byte-equivalent `VZVirtualMachineConfiguration` and resume the
    /// saved machine state (`machine.vzs` beside this manifest).
    /// `None` (also the serde default, so pre-D7 manifests parse) ⇒
    /// cold-boot only — the save failed or predates warm support.
    #[serde(default)]
    pub(crate) warm: Option<WarmMachineState>,
}

/// ADR 0096 D7: the identity + effective config of the SAVED VM. One
/// nested optional block (all-or-nothing) rather than loose fields —
/// a partially-present identity is useless and shouldn't parse as
/// warm-eligible.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WarmMachineState {
    /// `VZGenericMachineIdentifier.dataRepresentation` bytes — the
    /// restore contract requires the restoring VM's identifier to
    /// match the saved one.
    pub(crate) machine_identifier: Vec<u8>,
    /// The pinned virtio-net MAC — same contract (a differing MAC
    /// reproduces the generic VZErrorRestore=12).
    pub(crate) mac_address: String,
    /// The EXACT kernel cmdline the saved VM booted with. Used as an
    /// equality GATE at restore, never blindly reused: a resumed guest's
    /// in-memory state (e.g. its ENGRAM_EGRESS DNAT rules) reflects the
    /// save-time cmdline, and only a cold reboot re-runs the init shim —
    /// so a cmdline that would differ today means cold, not warm.
    pub(crate) kernel_cmdline: String,
    /// The RESOLVED aux drives attached to the saved VM (content shas
    /// pinned). The warm config must re-attach exactly these staged
    /// files; `spec.aux_ro_drives` stays symbolic for the cold path's
    /// fresh re-resolution.
    pub(crate) aux_ro_drives: Vec<engram_core::types::sandbox::AuxRoDrive>,
    /// Resolved (not spec+default recomputed) sizing — a changed
    /// backend default with a zero-spec must gate to cold, not build a
    /// mismatched config.
    pub(crate) memory_mib: u32,
    pub(crate) vcpus: u32,
    /// ADR 0112: the saved VM's swap-drive size (device shape is part
    /// of the restore-equality contract). `None` = no swap drive.
    /// `serde(default)` for warm blocks written before the field.
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

/// ADR 0096 D7: filename of the saved machine state (memory + device
/// state) under `<dest>/`, present iff the manifest carries a `warm`
/// block AND the best-effort save succeeded. Host-local by contract —
/// the file is protected via the user's keychain (same-user, same-host
/// restore only) and is NEVER chunked to BlobStorage.
pub(crate) const MACHINE_STATE_FILENAME: &str = "machine.vzs";

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
    // ADR 0096 D7: the machine-state file is OPTIONAL (best-effort
    // save; absent on cold-only snapshots) — count it when present.
    if let Ok(md) = tokio::fs::metadata(dest.join(MACHINE_STATE_FILENAME)).await {
        size_bytes += md.len();
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
        }
    }

    /// ADR 0096 D7: a pre-D7 manifest (no `warm` key at all) must parse
    /// with `warm: None` — i.e. cold-boot-only, never an error. Pins the
    /// #[serde(default)] contract.
    #[test]
    fn pre_d7_manifest_without_warm_field_parses_cold() {
        let m = VzSnapshotManifest::new(SandboxId::new(), fake_spec(), None);
        let mut v: serde_json::Value = serde_json::to_value(&m).unwrap();
        v.as_object_mut().unwrap().remove("warm");
        let parsed: VzSnapshotManifest = serde_json::from_value(v).unwrap();
        assert!(
            parsed.warm.is_none(),
            "missing warm field must default to None"
        );
    }

    /// ADR 0096 D7: the warm block round-trips intact.
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
        assert_eq!(w.swap_mib, Some(64), "ADR 0112: swap shape survives");
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
