//! Inputs from the trusted disk-capture and boot-selection producers.
//! Structural validation is not a certificate of prior capture durability.
use serde::{Deserialize, Serialize};

use super::{
    ids::{SandboxId, SessionId, SnapshotId},
    manifest::ManifestRef,
    sandbox::{AuxRoDrive, SandboxSpec},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedColdPrepare {
    pub version: u32,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub boot_attempt: String,
    pub image: String,
    pub disk: ManagedColdDisk,
    pub kernel_sha256: String,
    /// Fully resolved, ordered boot drives. No current-stamp substitution.
    pub aux_ro_drives: Vec<AuxRoDrive>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedColdDisk {
    /// The session's private writable lineage, selected at this exact version.
    pub manifest: ManifestRef,
    pub sha256: String,
    pub size_bytes: u64,
    pub source: ManagedColdSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedColdSource {
    /// The trusted producer forks this approved base before preparation.
    FreshImage { base_manifest: ManifestRef },
    /// Only a durable, explicitly disk-only capture is eligible. The record
    /// digest binds its consistency outcome; the backend cannot infer it.
    Capture {
        capture_id: SnapshotId,
        session_id: SessionId,
        sandbox_id: SandboxId,
        binding_epoch: u64,
        record_digest: String,
    },
}

/// Prepared and quarantined only. This cannot be a final policy ACK and has no
/// physical epoch. The live owner must be checked again by the start producer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedColdPrepared {
    pub version: u32,
    pub request: ManagedColdPrepare,
    pub state: ManagedColdState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedColdState {
    Closed,
}

impl ManagedColdPrepare {
    pub fn validate(&self, spec: &SandboxSpec) -> Result<(), String> {
        let manifest_valid = |m: ManifestRef| !m.manifest_id.is_nil() && m.version > 0;
        let bounded = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        let source_valid = match &self.disk.source {
            ManagedColdSource::FreshImage { base_manifest } => {
                manifest_valid(*base_manifest)
                    && base_manifest.manifest_id != self.disk.manifest.manifest_id
            }
            ManagedColdSource::Capture {
                capture_id,
                session_id,
                sandbox_id,
                binding_epoch,
                record_digest,
            } => {
                !capture_id.as_uuid().is_nil()
                    && *session_id == self.session_id
                    && !sandbox_id.as_uuid().is_nil()
                    && *binding_epoch > 0
                    && *binding_epoch <= 9_007_199_254_740_991
                    && super::managed_deny::is_digest(record_digest)
            }
        };
        let drives_valid = self.aux_ro_drives.len() >= 3
            && self.aux_ro_drives.len() <= AuxRoDrive::RESERVED_SLOTS
            && self.aux_ro_drives.iter().enumerate().all(|(i, d)| {
                d.drive_id == AuxRoDrive::slot_drive_id(i)
                    && d.guest_mount == AuxRoDrive::slot_guest_mount(i)
                    && d.fs_type == "squashfs"
                    && d.sha256.as_deref().is_some_and(is_sha256)
            });
        if self.version != 1
            || self.session_id.as_uuid().is_nil()
            || self.sandbox_id.as_uuid().is_nil()
            || !bounded(&self.boot_attempt)
            || self.image.is_empty()
            || self.image.len() > 256
            || self.image != spec.image
            || !manifest_valid(self.disk.manifest)
            || spec.rootfs_manifest != Some(self.disk.manifest)
            || spec.rootfs_source.is_none()
            || self.disk.size_bytes == 0
            || !is_sha256(&self.disk.sha256)
            || !is_sha256(&self.kernel_sha256)
            || !source_valid
            || !drives_valid
            || self.aux_ro_drives != spec.aux_ro_drives
            || spec.swap_source.is_some()
            || spec.swap_manifest.is_some()
        {
            return Err("invalid managed cold preparation".into());
        }
        Ok(())
    }
}

/// Canonical content digest used by staged boot files.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    fn inputs() -> (ManagedColdPrepare, SandboxSpec) {
        let disk = ManifestRef::new();
        let drives: Vec<_> = (0..3)
            .map(|i| AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(i),
                guest_mount: AuxRoDrive::slot_guest_mount(i),
                fs_type: "squashfs".into(),
                sha256: Some("a".repeat(64)),
            })
            .collect();
        let spec = SandboxSpec {
            image: "image-v1".into(),
            rootfs_source: Some("disk.ext4".into()),
            rootfs_manifest: Some(disk),
            image_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 128 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: drives.clone(),
            swap_mib: None,
            swap_source: None,
            swap_manifest: None,
        };
        let request = ManagedColdPrepare {
            version: 1,
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            boot_attempt: "boot-1".into(),
            image: spec.image.clone(),
            disk: ManagedColdDisk {
                manifest: disk,
                sha256: "b".repeat(64),
                size_bytes: 4096,
                source: ManagedColdSource::FreshImage {
                    base_manifest: ManifestRef::new(),
                },
            },
            kernel_sha256: "c".repeat(64),
            aux_ro_drives: drives,
        };
        (request, spec)
    }
    #[test]
    fn rejects_substituted_disk_and_unresolved_boot_inputs() {
        let (request, mut spec) = inputs();
        assert!(request.validate(&spec).is_ok());
        spec.rootfs_manifest = Some(request.disk.manifest.next_version());
        assert!(
            request.validate(&spec).is_err(),
            "newer disk must not replace selected disk"
        );
        spec.rootfs_manifest = Some(request.disk.manifest);
        spec.aux_ro_drives[1].sha256 = None;
        assert!(request.validate(&spec).is_err());
    }
    #[test]
    fn fresh_disk_must_have_private_lineage_and_capture_must_match_session() {
        let (mut request, spec) = inputs();
        request.disk.source = ManagedColdSource::FreshImage {
            base_manifest: request.disk.manifest,
        };
        assert!(
            request.validate(&spec).is_err(),
            "shared base is not private lineage"
        );
        request.disk.source = ManagedColdSource::Capture {
            capture_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
            record_digest: format!("sha256:{}", "d".repeat(64)),
        };
        assert!(request.validate(&spec).is_err());
    }
    #[test]
    fn legacy_missing_source_and_unknown_mode_cannot_decode() {
        let (request, _) = inputs();
        let mut encoded = serde_json::to_value(request).unwrap();
        encoded["disk"].as_object_mut().unwrap().remove("source");
        assert!(serde_json::from_value::<ManagedColdPrepare>(encoded.clone()).is_err());
        encoded["disk"]["source"] = serde_json::json!({"kind":"warm"});
        assert!(serde_json::from_value::<ManagedColdPrepare>(encoded).is_err());
    }
}
