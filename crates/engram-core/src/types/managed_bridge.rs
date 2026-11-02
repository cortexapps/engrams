//! Durable input selection and identity-only control of managed boot producers.
use super::{
    managed_assets::*, managed_boot::*, managed_create::*, manifest::ManifestRef,
    sandbox::AuxRoDrive,
};
use crate::{HostId, MetaError, SandboxId, SnapshotId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCreationQuery {
    pub identity: ManagedCreateIdentity,
    pub native_version: u32,
    pub native_digest: String,
}
impl ManagedCreationQuery {
    pub fn validate(&self) -> Result<(), MetaError> {
        self.identity.validate()?;
        if self.native_version != NATIVE_MANIFEST_VERSION
            || !super::managed_deny::is_digest(&self.native_digest)
        {
            return Err(conflict());
        }
        Ok(())
    }
    pub fn matches(&self, c: &ManagedCreation) -> Result<(), MetaError> {
        self.validate()?;
        if self.identity != c.identity
            || self.native_version != c.native.version
            || self.native_digest != c.native.digest
        {
            return Err(conflict());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootSelection {
    pub image: String,
    pub image_digest: String,
    pub base_manifest: ManifestRef,
    pub aux_ro_drives: Vec<AuxRoDrive>,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
}
impl ManagedBootSelection {
    pub fn validate(&self) -> Result<(), MetaError> {
        if self.vcpus == 0
            || self.memory_mib == 0
            || self.disk_gib == 0
            || self.image.is_empty()
            || self.image.len() > 256
            || !super::managed_deny::is_digest(&self.image_digest)
            || self.base_manifest.manifest_id.is_nil()
            || self.base_manifest.version == 0
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
            return Err(conflict());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRecoveryInput {
    pub query: ManagedCreationQuery,
    pub expected_source_key: ManagedBootKey,
    pub recovery_id: String,
}
impl ManagedRecoveryInput {
    pub fn validate(&self) -> Result<(), MetaError> {
        self.query.validate()?;
        self.expected_source_key.validate()?;
        if self.expected_source_key.creation != self.query.identity
            || self.expected_source_key.native_digest != self.query.native_digest
            || !bounded_id(&self.recovery_id)
        {
            return Err(conflict());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRecoveryIntent {
    pub recovery_id: String,
    pub source: ManagedBootKey,
    pub capture_id: SnapshotId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootIntent {
    pub query: ManagedCreationQuery,
    pub intent_id: String,
    pub revision: u64,
    pub selection: ManagedBootSelection,
    pub sandbox_id: SandboxId,
    pub binding_epoch: u64,
    pub disk_manifest: ManifestRef,
    pub recovery: Option<ManagedRecoveryIntent>,
    pub host_id: Option<HostId>,
    pub assets: Option<ManagedColdAssetsReceipt>,
    pub completed: bool,
}
impl ManagedBootIntent {
    pub fn operation_key(&self) -> String {
        self.recovery
            .as_ref()
            .map(|r| format!("recover:{}", r.recovery_id))
            .unwrap_or_else(|| "initial".into())
    }
    pub fn validate(&self) -> Result<(), MetaError> {
        self.query.validate()?;
        self.selection.validate()?;
        if !bounded_id(&self.intent_id)
            || self.revision == 0
            || self.sandbox_id.as_uuid().is_nil()
            || self.binding_epoch == 0
            || self.binding_epoch > 9_007_199_254_740_991
            || self.disk_manifest.manifest_id.is_nil()
            || self.disk_manifest.version != 1
        {
            return Err(conflict());
        }
        if let Some(r) = &self.recovery {
            ManagedRecoveryInput {
                query: self.query.clone(),
                expected_source_key: r.source.clone(),
                recovery_id: r.recovery_id.clone(),
            }
            .validate()?;
            if r.capture_id.as_uuid().is_nil()
                || self.binding_epoch
                    != r.source
                        .context
                        .target
                        .binding_epoch
                        .checked_add(1)
                        .ok_or_else(conflict)?
                || self.disk_manifest.manifest_id != r.capture_id.as_uuid()
            {
                return Err(conflict());
            }
        } else if self.binding_epoch != 1
            || self.disk_manifest.manifest_id == self.selection.base_manifest.manifest_id
        {
            return Err(conflict());
        }
        if let Some(a) = &self.assets {
            a.validate_for(&self.assets_request()?)?;
        }
        Ok(())
    }
    pub fn assets_request(&self) -> Result<ManagedColdAssetsRequest, MetaError> {
        Ok(ManagedColdAssetsRequest {
            version: 1,
            host_id: self.host_id.ok_or_else(conflict)?,
            session_id: self.query.identity.session_id,
            intent_id: self.intent_id.clone(),
            image: self.selection.image.clone(),
            image_digest: self.selection.image_digest.clone(),
            disk_manifest: self.disk_manifest,
            aux_ro_drives: self.selection.aux_ro_drives.clone(),
        })
    }
    pub fn view(&self) -> ManagedBootOperation {
        ManagedBootOperation {
            intent_id: self.intent_id.clone(),
            revision: self.revision,
            recovery_id: self.recovery.as_ref().map(|r| r.recovery_id.clone()),
            source_key: self.recovery.as_ref().map(|r| r.source.clone()),
            capture_id: self.recovery.as_ref().map(|r| r.capture_id),
            completed: self.completed,
        }
    }
    pub fn advance(&self, change: ManagedIntentAdvance) -> Result<Self, MetaError> {
        if self.completed {
            return Err(conflict());
        }
        let mut next = self.clone();
        match change {
            ManagedIntentAdvance::SelectHost(h)
                if self.host_id.is_none() && !h.as_uuid().is_nil() =>
            {
                next.host_id = Some(h)
            }
            ManagedIntentAdvance::Assets(a) if self.assets.is_none() => {
                a.validate_for(&self.assets_request()?)?;
                next.assets = Some(a);
            }
            ManagedIntentAdvance::Complete if self.assets.is_some() => next.completed = true,
            _ => return Err(conflict()),
        }
        next.revision = next.revision.checked_add(1).ok_or_else(conflict)?;
        next.validate()?;
        Ok(next)
    }
}
#[derive(Clone, Debug)]
pub enum ManagedIntentAdvance {
    SelectHost(HostId),
    Assets(ManagedColdAssetsReceipt),
    Complete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootOperation {
    pub intent_id: String,
    pub revision: u64,
    pub recovery_id: Option<String>,
    pub source_key: Option<ManagedBootKey>,
    pub capture_id: Option<SnapshotId>,
    pub completed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCreationPhase {
    Absent,
    Held,
    Preparing,
    Boot,
    Recovering,
    Terminal,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCreationState {
    pub query: ManagedCreationQuery,
    pub state: ManagedCreationPhase,
    pub boot: Option<ManagedBootRecord>,
    pub operation: Option<ManagedBootOperation>,
    pub terminal_reason: Option<String>,
}
pub fn bounded_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
pub fn conflict() -> MetaError {
    MetaError::Conflict("managed creation authority changed".into())
}

impl ManagedCreationState {
    pub fn from_parts(
        query: ManagedCreationQuery,
        status: Option<super::session::SessionState>,
        creation: Option<&ManagedCreation>,
        mut boot: Option<ManagedBootRecord>,
        intent: Option<&ManagedBootIntent>,
    ) -> Result<Self, MetaError> {
        query.validate()?;
        if let Some(c) = creation {
            query.matches(c)?;
        } else if status.is_some() || boot.is_some() || intent.is_some() {
            return Err(conflict());
        }
        if creation.is_some() && status.is_none() {
            return Err(conflict());
        }
        if let Some(b) = &mut boot {
            if b.request.key.creation != query.identity
                || b.request.key.native_digest != query.native_digest
            {
                return Err(conflict());
            }
            b.sealed_agent = None;
        }
        if intent.is_some_and(|i| i.query != query) {
            return Err(conflict());
        }
        let terminal = status.filter(|s| {
            matches!(
                s,
                super::session::SessionState::Dead
                    | super::session::SessionState::Failed
                    | super::session::SessionState::Completed
            )
        });
        let state = if status.is_none() {
            ManagedCreationPhase::Absent
        } else if terminal.is_some() {
            ManagedCreationPhase::Terminal
        } else if intent.is_some_and(|i| !i.completed) {
            if intent.is_some_and(|i| i.recovery.is_some()) {
                ManagedCreationPhase::Recovering
            } else {
                ManagedCreationPhase::Preparing
            }
        } else if boot.is_some() {
            ManagedCreationPhase::Boot
        } else {
            ManagedCreationPhase::Held
        };
        Ok(Self {
            query,
            state,
            boot,
            operation: intent.map(|i| i.view()),
            terminal_reason: terminal.map(|s| s.as_str().to_owned()),
        })
    }
}

impl ManagedBootIntent {
    pub fn validate_claim(&self, r: &ManagedBootPrepare) -> Result<(), MetaError> {
        self.validate()?;
        let a = self.assets.as_ref().ok_or_else(conflict)?;
        let spec = r.spec()?;
        let t = &r.key.context.target;
        if r.key.creation != self.query.identity
            || r.key.native_digest != self.query.native_digest
            || r.key.boot_attempt != self.intent_id
            || t.sandbox_id != self.sandbox_id
            || Some(t.host_id) != self.host_id
            || t.binding_epoch != self.binding_epoch
            || r.cold.image != self.selection.image
            || r.cold.aux_ro_drives != self.selection.aux_ro_drives
            || r.cold.disk.manifest != self.disk_manifest
            || r.cold.disk.sha256 != a.disk_sha256
            || r.cold.disk.size_bytes != a.disk_size_bytes
            || r.cold.kernel_sha256 != a.kernel_sha256
            || spec.cpu.vcpus != self.selection.vcpus
            || spec.memory.max_mib != self.selection.memory_mib
            || spec.disk.max_gib != self.selection.disk_gib
        {
            return Err(conflict());
        }
        match (&self.recovery, &r.cold.disk.source) {
            (None, super::managed_cold::ManagedColdSource::FreshImage { base_manifest })
                if *base_manifest == self.selection.base_manifest => {}
            (
                Some(recovery),
                super::managed_cold::ManagedColdSource::Capture { capture_id, .. },
            ) if *capture_id == recovery.capture_id => {}
            _ => return Err(conflict()),
        }
        Ok(())
    }
}
