//! Durable coordinator authority and exact host commands for a closed boot.
use serde::{Deserialize, Serialize};

use super::{
    managed_cold::ManagedColdPrepare,
    managed_create::{digest, FrozenNativeManifest, ManagedCreateIdentity},
    managed_deny::{is_digest, ManagedAppliedState, ManagedPolicyContext},
    sandbox::{AgentSpec, SandboxSpec},
};
use crate::MetaError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootKey {
    pub creation: ManagedCreateIdentity,
    /// Registered physical placement. Binding hash is supplied only at finalization.
    pub context: ManagedPolicyContext,
    pub boot_attempt: String,
    pub native_digest: String,
}
impl ManagedBootKey {
    pub fn validate(&self) -> Result<(), MetaError> {
        self.creation.validate()?;
        self.context.validate().map_err(MetaError::Conflict)?;
        if self.context.target.session_id != self.creation.session_id
            || self.context.binding_hash.is_some()
            || !is_digest(&self.native_digest)
            || self.boot_attempt.is_empty()
            || self.boot_attempt.len() > 128
            || !self
                .boot_attempt
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(conflict());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootPrepare {
    pub key: ManagedBootKey,
    pub cold: ManagedColdPrepare,
    pub bootstrap_policy: super::managed_deny::ManagedDenyPolicy,
    /// Exact persisted capture receipt. A fresh image has no capture authority.
    pub capture_authority: Option<ManagedCaptureAuthority>,
    /// Exact, non-secret SandboxSpec JSON. Decode through `spec`, never a profile.
    pub sandbox_spec_json: String,
}
impl ManagedBootPrepare {
    pub fn spec(&self) -> Result<SandboxSpec, MetaError> {
        self.key.validate()?;
        self.bootstrap_policy
            .validate()
            .map_err(MetaError::Conflict)?;
        if self.sandbox_spec_json.len() > 1024 * 1024 {
            return Err(conflict());
        }
        let spec: SandboxSpec =
            serde_json::from_str(&self.sandbox_spec_json).map_err(|_| conflict())?;
        self.cold.validate(&spec).map_err(MetaError::Conflict)?;
        if !spec.env.is_empty()
            || self.cold.session_id != self.key.creation.session_id
            || self.cold.sandbox_id != self.key.context.target.sandbox_id
            || self.cold.boot_attempt != self.key.boot_attempt
        {
            return Err(conflict());
        }
        match (&self.cold.disk.source, &self.capture_authority) {
            (super::managed_cold::ManagedColdSource::FreshImage { .. }, None) => {}
            (super::managed_cold::ManagedColdSource::Capture { .. }, Some(authority)) => {
                authority.validate_recovery(self)?;
            }
            _ => return Err(conflict()),
        }
        Ok(spec)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootPreparedReceipt {
    pub request: ManagedBootPrepare,
    pub host_incarnation: String,
    pub network_identity: String,
    pub receipt_id: String,
}
impl ManagedBootPreparedReceipt {
    pub fn validate(&self) -> Result<(), MetaError> {
        self.request.spec()?;
        if self.host_incarnation.is_empty()
            || self.host_incarnation.len() > 128
            || !is_digest(&self.network_identity)
            || !is_digest(&self.receipt_id)
        {
            return Err(conflict());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootFinalization {
    pub prepared: ManagedBootPreparedReceipt,
    pub policy: ManagedAppliedState,
    pub native_version: u32,
    pub combined_manifest: Vec<u8>,
    pub combined_manifest_digest: String,
    /// Digest of the canonical full AgentSpec, including its environment.
    pub agent_digest: String,
}
impl ManagedBootFinalization {
    pub fn validate(&self, native: &FrozenNativeManifest) -> Result<(), MetaError> {
        self.prepared.validate()?;
        self.policy.validate().map_err(MetaError::Conflict)?;
        let key = &self.prepared.request.key;
        let mut context = key.context.clone();
        context.binding_hash = self.policy.context.binding_hash.clone();
        if context.binding_hash.is_none()
            || context != self.policy.context
            || self.policy.host_incarnation != self.prepared.host_incarnation
            || self
                .policy
                .network_proof
                .as_ref()
                .map(|p| &p.network_identity)
                != Some(&self.prepared.network_identity)
            || native.version != self.native_version
            || native.digest != key.native_digest
            || self.combined_manifest_digest != digest(&self.combined_manifest)
            || !is_digest(&self.agent_digest)
        {
            return Err(conflict());
        }
        validate_combined_manifest(native, &self.combined_manifest)
    }
    pub fn digest(&self) -> Result<String, MetaError> {
        serde_json::to_vec(self)
            .map(|v| digest(&v))
            .map_err(|_| conflict())
    }
}

/// Transport-only plaintext. The coordinator persists only its sealed form.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootFinalize {
    pub finalization: ManagedBootFinalization,
    pub agent: AgentSpec,
}
impl ManagedBootFinalize {
    pub fn validate_agent(&self) -> Result<(), MetaError> {
        let f = &self.finalization;
        let value = serde_json::to_value(&self.agent).map_err(|_| conflict())?;
        let bytes = serde_json::to_vec(&value).map_err(|_| conflict())?;
        if digest(&bytes) != f.agent_digest
            || self.agent.binding_epoch != f.prepared.request.key.context.target.binding_epoch
            || self.agent.argv.is_empty()
            || self.agent.env.get("ENGRAM_TOOLS").map(String::as_bytes)
                != Some(f.combined_manifest.as_slice())
            || self.agent.session_env.contains_key("ENGRAM_TOOLS")
            || self
                .agent
                .env
                .keys()
                .chain(self.agent.session_env.keys())
                .any(|k| k.starts_with("ENGRAM_INITIAL_PROMPT"))
        {
            return Err(conflict());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootFinalReceipt {
    pub key: ManagedBootKey,
    pub prepared_receipt_id: String,
    pub finalization_digest: String,
    pub host_incarnation: String,
    pub network_identity: String,
    pub receipt_id: String,
}
impl ManagedBootFinalReceipt {
    pub fn validate_for(&self, f: &ManagedBootFinalization) -> Result<(), MetaError> {
        if self.key != f.prepared.request.key
            || self.prepared_receipt_id != f.prepared.receipt_id
            || self.finalization_digest != f.digest()?
            || self.host_incarnation != f.prepared.host_incarnation
            || self.network_identity != f.prepared.network_identity
            || !is_digest(&self.receipt_id)
        {
            return Err(conflict());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootStart {
    pub receipt: ManagedBootFinalReceipt,
    pub start_id: String,
}
impl ManagedBootStart {
    pub fn validate(&self) -> Result<(), MetaError> {
        self.receipt.key.validate()?;
        if !is_digest(&self.start_id) {
            return Err(conflict());
        }
        Ok(())
    }
}

/// A read is bound to the complete key. Missing is never permission to start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedBootHostState {
    Missing { key: ManagedBootKey },
    Prepared { receipt: ManagedBootPreparedReceipt },
    Finalized { receipt: ManagedBootFinalReceipt },
    StartPending { intent: ManagedBootStart },
    Started { intent: ManagedBootStart },
    Failed { key: ManagedBootKey },
    Captured { authority: ManagedCaptureAuthority },
}

pub fn validate_combined_manifest(
    native: &FrozenNativeManifest,
    bytes: &[u8],
) -> Result<(), MetaError> {
    native.validate()?;
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return Err(conflict());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| conflict())?;
    let entries = value.as_array().ok_or_else(conflict)?;
    let original: Vec<serde_json::Value> =
        serde_json::from_slice(&native.bytes).map_err(|_| conflict())?;
    if entries.len() > 128 || !entries.starts_with(&original) {
        return Err(conflict());
    }
    let mut names = std::collections::BTreeSet::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(conflict)?;
        if !names.insert(name)
            || !entry.get("description").is_some_and(|v| v.is_string())
            || !entry.get("inputSchema").is_some_and(|v| v.is_object())
        {
            return Err(conflict());
        }
    }
    Ok(())
}
pub fn conflict() -> MetaError {
    MetaError::Conflict("managed boot authority changed or unavailable".into())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedBootPhase {
    Claimed,
    Prepared,
    Finalizing,
    Finalized,
    StartIssued,
    Started,
    Interrupted,
}
impl ManagedBootPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Prepared => "prepared",
            Self::Finalizing => "finalizing",
            Self::Finalized => "finalized",
            Self::StartIssued => "start_issued",
            Self::Started => "started",
            Self::Interrupted => "interrupted",
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootSealedAgent {
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootRecord {
    pub request: ManagedBootPrepare,
    pub revision: u64,
    pub phase: ManagedBootPhase,
    pub prepared: Option<ManagedBootPreparedReceipt>,
    pub finalization: Option<ManagedBootFinalization>,
    pub sealed_agent: Option<ManagedBootSealedAgent>,
    pub write_policy: Option<super::mcp_policy::TrustedWritePolicy>,
    pub receipt: Option<ManagedBootFinalReceipt>,
    pub start: Option<ManagedBootStart>,
}
/// Transactional events; no caller can assign arbitrary state or mutate a claim.
#[derive(Clone)]
pub enum ManagedBootAdvance {
    Prepared(ManagedBootPreparedReceipt),
    Finalize {
        finalization: Box<ManagedBootFinalization>,
        sealed_agent: ManagedBootSealedAgent,
        write_policy: super::mcp_policy::TrustedWritePolicy,
    },
    Finalized(ManagedBootFinalReceipt),
    IssueStart(ManagedBootStart),
    Started(ManagedBootStart),
    Interrupt,
}
impl ManagedBootRecord {
    pub fn claimed(request: ManagedBootPrepare) -> Self {
        Self {
            request,
            revision: 1,
            phase: ManagedBootPhase::Claimed,
            prepared: None,
            finalization: None,
            sealed_agent: None,
            write_policy: None,
            receipt: None,
            start: None,
        }
    }
    pub fn advance(
        &self,
        event: ManagedBootAdvance,
        native: &FrozenNativeManifest,
    ) -> Result<Self, MetaError> {
        let mut next = self.clone();
        match event {
            ManagedBootAdvance::Prepared(r) => {
                r.validate()?;
                if self.phase != ManagedBootPhase::Claimed || r.request != self.request {
                    return Err(conflict());
                }
                next.prepared = Some(r);
                next.phase = ManagedBootPhase::Prepared;
            }
            ManagedBootAdvance::Finalize {
                finalization: f,
                sealed_agent,
                write_policy,
            } => {
                f.validate(native)?;
                if self.phase != ManagedBootPhase::Prepared
                    || Some(&f.prepared) != self.prepared.as_ref()
                    || sealed_agent.nonce.len() != 12
                    || sealed_agent.wrapped_dek.is_empty()
                    || sealed_agent.ciphertext.is_empty()
                    || sealed_agent.key_id.is_empty()
                {
                    return Err(conflict());
                }
                next.write_policy = Some(write_policy);
                next.finalization = Some(*f);
                next.sealed_agent = Some(sealed_agent);
                next.phase = ManagedBootPhase::Finalizing;
            }
            ManagedBootAdvance::Finalized(r) => {
                if self.phase != ManagedBootPhase::Finalizing {
                    return Err(conflict());
                }
                r.validate_for(self.finalization.as_ref().ok_or_else(conflict)?)?;
                next.receipt = Some(r);
                next.phase = ManagedBootPhase::Finalized;
            }
            ManagedBootAdvance::IssueStart(i) => {
                i.validate()?;
                if self.phase != ManagedBootPhase::Finalized
                    || Some(&i.receipt) != self.receipt.as_ref()
                {
                    return Err(conflict());
                }
                next.start = Some(i);
                next.phase = ManagedBootPhase::StartIssued;
            }
            ManagedBootAdvance::Started(i) => {
                if self.phase != ManagedBootPhase::StartIssued || Some(&i) != self.start.as_ref() {
                    return Err(conflict());
                }
                next.phase = ManagedBootPhase::Started;
            }
            ManagedBootAdvance::Interrupt => {
                if self.phase == ManagedBootPhase::Interrupted {
                    return Err(conflict());
                }
                next.phase = ManagedBootPhase::Interrupted;
            }
        }
        next.revision = self.revision.checked_add(1).ok_or_else(conflict)?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combined_manifest_keeps_all_native_fields_and_order() {
        let bytes=br#"[{"name":"native","description":"n","inputSchema":{},"execution":"sync","custom":{"x":1}}]"#;
        let native = FrozenNativeManifest {
            version: 1,
            bytes: bytes.to_vec(),
            digest: digest(bytes),
        };
        let mut entries: Vec<serde_json::Value> = serde_json::from_slice(bytes).unwrap();
        entries.push(serde_json::json!({"name":"remote","description":"r","inputSchema":{},"execution":"deferred","resultFormat":"mcp"}));
        validate_combined_manifest(&native, &serde_json::to_vec(&entries).unwrap()).unwrap();
        let mut wrong = entries.clone();
        wrong[0].as_object_mut().unwrap().remove("custom");
        assert!(validate_combined_manifest(&native, &serde_json::to_vec(&wrong).unwrap()).is_err());
        let mut wrong = entries.clone();
        wrong.reverse();
        assert!(validate_combined_manifest(&native, &serde_json::to_vec(&wrong).unwrap()).is_err());
        let mut wrong = entries.clone();
        wrong[1]["name"] = "native".into();
        assert!(validate_combined_manifest(&native, &serde_json::to_vec(&wrong).unwrap()).is_err());
        assert!(validate_combined_manifest(&native, &vec![b' '; 1024 * 1024 + 1]).is_err());
        let mut many = entries;
        for i in 2..129 {
            many.push(
                serde_json::json!({"name":format!("t{i}"),"description":"r","inputSchema":{}}),
            );
        }
        assert!(validate_combined_manifest(&native, &serde_json::to_vec(&many).unwrap()).is_err());
    }
}

impl std::fmt::Debug for ManagedBootSealedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManagedBootSealedAgent([sealed])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCaptureRequest {
    pub key: ManagedBootKey,
    pub capture_id: crate::SnapshotId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedDiskConsistency {
    CrashConsistentDiskCutV1,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCapturedSourceState {
    Destroyed,
}
/// Returned only by the authenticated host capture owner. A digest is an integrity
/// check, not provenance. The coordinator persists this under the registered source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCaptureAuthority {
    pub version: u32,
    pub request: ManagedCaptureRequest,
    pub host_incarnation: String,
    pub disk_manifest: super::manifest::ManifestRef,
    pub disk_sha256: String,
    pub disk_size_bytes: u64,
    pub image: String,
    pub kernel_sha256: String,
    pub aux_ro_drives: Vec<super::sandbox::AuxRoDrive>,
    pub consistency: ManagedDiskConsistency,
    pub source_state: ManagedCapturedSourceState,
    pub record_digest: String,
}
impl ManagedCaptureAuthority {
    pub fn digest(&self) -> Result<String, MetaError> {
        let mut value = self.clone();
        value.record_digest.clear();
        serde_json::to_vec(&value)
            .map(|v| digest(&v))
            .map_err(|_| conflict())
    }
    pub fn validate(&self) -> Result<(), MetaError> {
        self.request.key.validate()?;
        if self.version != 1
            || self.request.capture_id.as_uuid().is_nil()
            || self.host_incarnation.is_empty()
            || self.host_incarnation.len() > 128
            || self.disk_manifest.manifest_id.is_nil()
            || self.disk_manifest.version == 0
            || self.disk_size_bytes == 0
            || !super::managed_cold::is_sha256(&self.disk_sha256)
            || !super::managed_cold::is_sha256(&self.kernel_sha256)
            || self.image.is_empty()
            || self.image.len() > 256
            || self.record_digest != self.digest()?
        {
            return Err(conflict());
        }
        Ok(())
    }
    pub fn validate_recovery(&self, next: &ManagedBootPrepare) -> Result<(), MetaError> {
        self.validate()?;
        next.key.validate()?;
        let old = &self.request.key;
        let key = &next.key;
        let cold = &next.cold;
        if key.creation != old.creation
            || key.native_digest != old.native_digest
            || key.context.target.binding_epoch
                != old
                    .context
                    .target
                    .binding_epoch
                    .checked_add(1)
                    .ok_or_else(conflict)?
            || key.boot_attempt == old.boot_attempt
            || key.context.target.sandbox_id == old.context.target.sandbox_id
            || cold.image != self.image
            || cold.disk.manifest != self.disk_manifest
            || cold.disk.sha256 != self.disk_sha256
            || cold.disk.size_bytes != self.disk_size_bytes
            || cold.kernel_sha256 != self.kernel_sha256
            || cold.aux_ro_drives != self.aux_ro_drives
            || cold.disk.source
                != (super::managed_cold::ManagedColdSource::Capture {
                    capture_id: self.request.capture_id,
                    session_id: old.creation.session_id,
                    sandbox_id: old.context.target.sandbox_id,
                    binding_epoch: old.context.target.binding_epoch,
                    record_digest: self.record_digest.clone(),
                })
        {
            return Err(conflict());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedBootFinalizeInput {
    pub key: ManagedBootKey,
    pub prepared_receipt_id: String,
    pub policy: ManagedAppliedState,
    pub native_version: u32,
    pub combined_manifest: Vec<u8>,
    pub combined_manifest_digest: String,
}
