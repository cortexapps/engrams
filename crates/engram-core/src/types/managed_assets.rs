//! Authenticated, read-only cold boot asset inspection. No VM or boot authority.
use super::{manifest::ManifestRef, sandbox::AuxRoDrive};
use crate::{HostId, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedColdAssetsRequest {
    pub version: u32,
    pub host_id: HostId,
    pub session_id: SessionId,
    pub intent_id: String,
    pub image: String,
    pub image_digest: String,
    pub disk_manifest: ManifestRef,
    pub aux_ro_drives: Vec<AuxRoDrive>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedColdBackend {
    Firecracker,
    Vz,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedColdAssetsReceipt {
    pub request: ManagedColdAssetsRequest,
    pub host_incarnation: String,
    pub backend: ManagedColdBackend,
    pub kernel_sha256: String,
    pub disk_sha256: String,
    pub disk_size_bytes: u64,
}

impl ManagedColdAssetsRequest {
    pub fn validate(&self) -> Result<(), crate::MetaError> {
        let bounded = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if self.version != 1
            || self.host_id.as_uuid().is_nil()
            || self.session_id.as_uuid().is_nil()
            || !bounded(&self.intent_id)
            || self.image.is_empty()
            || self.image.len() > 256
            || !super::managed_deny::is_digest(&self.image_digest)
            || self.disk_manifest.manifest_id.is_nil()
            || self.disk_manifest.version == 0
            || self.aux_ro_drives.len() < 3
            || self.aux_ro_drives.len() > AuxRoDrive::RESERVED_SLOTS
            || self.aux_ro_drives.iter().enumerate().any(|(i, d)| {
                d.drive_id != AuxRoDrive::slot_drive_id(i)
                    || d.guest_mount != AuxRoDrive::slot_guest_mount(i)
                    || d.fs_type != "squashfs"
                    || !d
                        .sha256
                        .as_deref()
                        .is_some_and(super::managed_cold::is_sha256)
            })
        {
            return Err(crate::MetaError::Conflict(
                "invalid managed cold assets".into(),
            ));
        }
        Ok(())
    }
}
impl ManagedColdAssetsReceipt {
    /// Integrity and association only. Provenance comes from authenticated host transport.
    pub fn validate_for(&self, request: &ManagedColdAssetsRequest) -> Result<(), crate::MetaError> {
        request.validate()?;
        if &self.request != request
            || self.host_incarnation.is_empty()
            || self.host_incarnation.len() > 128
            || self.disk_size_bytes == 0
            || !super::managed_cold::is_sha256(&self.disk_sha256)
            || !super::managed_cold::is_sha256(&self.kernel_sha256)
        {
            return Err(crate::MetaError::Conflict(
                "managed cold assets association mismatch".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assets_reject_substitution_and_unresolved_drives() {
        let r = ManagedColdAssetsRequest {
            version: 1,
            host_id: HostId::new(),
            session_id: SessionId::new(),
            intent_id: "intent-a".into(),
            image: "test.invalid/image:v1".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            disk_manifest: ManifestRef::new(),
            aux_ro_drives: (0..3)
                .map(|i| AuxRoDrive {
                    drive_id: AuxRoDrive::slot_drive_id(i),
                    guest_mount: AuxRoDrive::slot_guest_mount(i),
                    fs_type: "squashfs".into(),
                    sha256: Some("b".repeat(64)),
                })
                .collect(),
        };
        let a = ManagedColdAssetsReceipt {
            request: r.clone(),
            host_incarnation: "incarnation-a".into(),
            backend: ManagedColdBackend::Firecracker,
            kernel_sha256: "c".repeat(64),
            disk_sha256: "d".repeat(64),
            disk_size_bytes: 4096,
        };
        r.validate().unwrap();
        a.validate_for(&r).unwrap();
        let mut bad = r.clone();
        bad.aux_ro_drives.swap(0, 1);
        assert!(bad.validate().is_err());
        let mut bad = r.clone();
        bad.disk_manifest = bad.disk_manifest.next_version();
        assert!(a.validate_for(&bad).is_err());
        let mut bad = a.clone();
        bad.host_incarnation.clear();
        assert!(bad.validate_for(&r).is_err());
        let mut bad = a.clone();
        bad.kernel_sha256 = "a".repeat(63);
        assert!(bad.validate_for(&r).is_err());
        let mut value = serde_json::to_value(&r).unwrap();
        value["disk_path"] = "/untrusted".into();
        assert!(serde_json::from_value::<ManagedColdAssetsRequest>(value).is_err());
    }
}
