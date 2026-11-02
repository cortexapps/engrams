//! Immutable native inputs for a held managed session. No boot authority.
use super::mcp_policy::McpPolicyCreation;
use crate::{MetaError, SessionId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const NATIVE_MANIFEST_VERSION: u32 = 1;
pub const MAX_NATIVE_MANIFEST_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCreateIdentity {
    pub session_id: SessionId,
    pub task_id: String,
    pub policy: McpPolicyCreation,
    pub input_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenNativeManifest {
    pub version: u32,
    pub bytes: Vec<u8>,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCreation {
    pub identity: ManagedCreateIdentity,
    pub native: FrozenNativeManifest,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

impl FrozenNativeManifest {
    pub fn validate(&self) -> Result<(), MetaError> {
        let invalid = || MetaError::Conflict("invalid frozen native manifest".into());
        if self.version != NATIVE_MANIFEST_VERSION
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_NATIVE_MANIFEST_BYTES
            || self.digest != digest(&self.bytes)
        {
            return Err(invalid());
        }
        let value: serde_json::Value =
            serde_json::from_slice(&self.bytes).map_err(|_| invalid())?;
        let entries = value.as_array().ok_or_else(invalid)?;
        if entries.len() > 128 {
            return Err(invalid());
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in entries {
            let obj = entry.as_object().ok_or_else(invalid)?;
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .ok_or_else(invalid)?;
            if !names.insert(name)
                || !obj.get("description").is_some_and(|v| v.is_string())
                || !obj.get("inputSchema").is_some_and(|v| v.is_object())
                || !matches!(
                    obj.get("execution").and_then(|v| v.as_str()),
                    Some("sync" | "deferred")
                )
                || obj.get("resultFormat").is_some_and(|v| v != "json")
            {
                return Err(invalid());
            }
            if let Some(bindings) = obj.get("nativeBindings") {
                let bindings = bindings.as_object().ok_or_else(invalid)?;
                if bindings.values().any(|v| !v.is_null() && !v.is_string()) {
                    return Err(invalid());
                }
            }
        }
        Ok(())
    }
}

impl ManagedCreateIdentity {
    pub fn validate(&self) -> Result<(), MetaError> {
        if [
            self.task_id.as_str(),
            self.policy.input_id.as_str(),
            self.policy.actor_id.as_str(),
        ]
        .iter()
        .any(|v| v.is_empty() || v.len() > 256)
            || self.policy.parent_session_id == Some(self.session_id)
            || !valid_digest(&self.input_digest)
        {
            return Err(MetaError::Conflict(
                "invalid managed creation identity".into(),
            ));
        }
        Ok(())
    }
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl ManagedCreation {
    pub fn validate_for(&self, ws: &crate::traits::SessionCreateWriteSet) -> Result<(), MetaError> {
        self.identity.validate()?;
        self.native.validate()?;
        if self.identity.session_id != ws.session_id
            || ws.mcp_policy.as_ref() != Some(&self.identity.policy)
            || ws.spec.mode != super::session::SessionMode::Agent
            || !matches!(
                ws.runtime_spec.selected_harness.as_deref(),
                Some("claude" | "codex")
            )
            || ws
                .sealed_secrets
                .as_ref()
                .is_some_and(|s| s.session_id != ws.session_id)
            || ws
                .oauth_binding
                .as_ref()
                .is_some_and(|b| b.session_id != ws.session_id)
        {
            return Err(MetaError::Conflict(
                "managed creation association mismatch".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native(bytes: &[u8]) -> FrozenNativeManifest {
        FrozenNativeManifest {
            version: 1,
            bytes: bytes.to_vec(),
            digest: digest(bytes),
        }
    }
    #[test]
    fn native_creation_rejects_bad_bytes_and_preserves_full_native_contract() {
        let bytes = br#" [ {"name":"mcp_native_tool","description":"native","inputSchema":{},"execution":"sync","nativeBindings":{"claude":"NativeTool","codex":"native_tool"},"extra":{"preserve":true}} ] "#;
        let manifest = native(bytes);
        manifest.validate().unwrap();
        assert_eq!(manifest.bytes, bytes);
        native(b"[]").validate().unwrap();
        for bytes in [b"".as_slice(), b"{}", b"[{}]", br#"[{"name":"a","description":"a","inputSchema":{},"execution":"deferred","resultFormat":"mcp"}]"#] {
            assert!(native(bytes).validate().is_err());
        }
        let mut bad = manifest.clone();
        bad.digest = digest(b"[]");
        assert!(bad.validate().is_err());
        let mut bad = manifest;
        bad.version = 2;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn native_creation_rejects_duplicates_and_limits() {
        let tool = serde_json::json!({"name":"tool","description":"x","inputSchema":{},"execution":"deferred"});
        assert!(native(&serde_json::to_vec(&vec![tool.clone(); 2]).unwrap())
            .validate()
            .is_err());
        let many: Vec<_> = (0..129)
            .map(|i| {
                let mut t = tool.clone();
                t["name"] = format!("tool_{i}").into();
                t
            })
            .collect();
        assert!(native(&serde_json::to_vec(&many).unwrap())
            .validate()
            .is_err());
        assert!(native(&vec![b' '; MAX_NATIVE_MANIFEST_BYTES + 1])
            .validate()
            .is_err());
    }
}
