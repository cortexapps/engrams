//! Exact managed authority policy. The digest matches the canonical MCP envelope.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedDenyPolicy {
    pub version: u32,
    pub revision: u64,
    pub hosts: Vec<String>,
    pub digest: String,
}

/// Accept DNS names only; normalize the same way for DNS, SNI and HTTP authority.
pub fn canonical_host(raw: &str) -> Option<String> {
    let host = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    if host.len() > 253
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    Some(host)
}

impl ManagedDenyPolicy {
    pub fn new(revision: u64, hosts: impl IntoIterator<Item = String>) -> Result<Self, String> {
        if revision > 9_007_199_254_740_991 {
            return Err("invalid managed policy revision".into());
        }
        let hosts: BTreeSet<_> = hosts
            .into_iter()
            .map(|h| canonical_host(&h).ok_or_else(|| "invalid managed host".to_string()))
            .collect::<Result<_, _>>()?;
        if hosts.len() > 4096 {
            return Err("managed host limit exceeded".into());
        }
        let mut result = Self {
            version: 1,
            revision,
            hosts: hosts.into_iter().collect(),
            digest: String::new(),
        };
        // These are canonical key order and integer encoding in the O producer.
        let bytes = format!(
            "{{\"hosts\":{},\"revision\":{},\"version\":1}}",
            serde_json::to_string(&result.hosts).map_err(|e| e.to_string())?,
            revision
        );
        result.digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes.as_bytes())));
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), String> {
        if Self::new(self.revision, self.hosts.clone())? != *self {
            return Err("invalid managed policy digest or canonical hosts".into());
        }
        Ok(())
    }
    pub fn denies(&self, host: &str) -> bool {
        canonical_host(host).is_some_and(|h| self.hosts.binary_search(&h).is_ok())
    }
    pub fn contains(&self, old: &Self) -> bool {
        old.hosts
            .iter()
            .all(|h| self.hosts.binary_search(h).is_ok())
    }
}

/// Physical placement and operator identity are supplied by trusted producers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedPolicyTarget {
    pub session_id: crate::SessionId,
    pub sandbox_id: crate::SandboxId,
    pub host_id: crate::HostId,
    pub binding_epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedPolicyContext {
    pub target: ManagedPolicyTarget,
    pub operator_generation: u64,
    pub operator_identity: String,
    /// None is bootstrap, never a final binding acknowledgement.
    pub binding_hash: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedNetworkProof {
    pub target: ManagedPolicyTarget,
    pub guest_ip: std::net::Ipv4Addr,
    pub network_identity: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedAppliedState {
    pub context: ManagedPolicyContext,
    pub policy: ManagedDenyPolicy,
    pub host_incarnation: String,
    pub proxy_incarnation: String,
    pub state_id: String,
    pub network_proof: Option<ManagedNetworkProof>,
}
impl ManagedAppliedState {
    pub fn new(
        context: ManagedPolicyContext,
        policy: ManagedDenyPolicy,
        host_incarnation: String,
        proxy_incarnation: String,
        network_proof: Option<ManagedNetworkProof>,
    ) -> Result<Self, String> {
        policy.validate()?;
        context.validate()?;
        if host_incarnation.is_empty()
            || host_incarnation.len() > 128
            || proxy_incarnation.is_empty()
            || proxy_incarnation.len() > 128
        {
            return Err("missing managed service incarnation".into());
        }
        if context.binding_hash.is_some() && network_proof.is_none() {
            return Err("final policy requires strict network proof".into());
        }
        if network_proof.as_ref().is_some_and(|p| {
            p.target != context.target
                || p.guest_ip.is_unspecified()
                || !is_digest(&p.network_identity)
        }) {
            return Err("invalid strict network proof".into());
        }
        let mut state = Self {
            context,
            policy,
            host_incarnation,
            proxy_incarnation,
            state_id: String::new(),
            network_proof,
        };
        state.state_id = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(
                serde_json::to_vec(&state).map_err(|e| e.to_string())?
            ))
        );
        Ok(state)
    }
    pub fn validate(&self) -> Result<(), String> {
        if Self::new(
            self.context.clone(),
            self.policy.clone(),
            self.host_incarnation.clone(),
            self.proxy_incarnation.clone(),
            self.network_proof.clone(),
        )? != *self
        {
            return Err("invalid managed receipt".into());
        }
        Ok(())
    }
}
impl ManagedPolicyContext {
    pub fn validate(&self) -> Result<(), String> {
        let t = &self.target;
        if t.session_id.as_uuid().is_nil()
            || t.sandbox_id.as_uuid().is_nil()
            || t.host_id.as_uuid().is_nil()
            || t.binding_epoch == 0
            || t.binding_epoch > 9_007_199_254_740_991
            || (self.operator_generation == 0 || self.operator_generation > 9_007_199_254_740_991)
            || !is_digest(&self.operator_identity)
            || self.binding_hash.as_ref().is_some_and(|h| !is_digest(h))
        {
            return Err("invalid managed context".into());
        }
        Ok(())
    }
}
pub fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedPolicyApply {
    pub expected: Option<ManagedAppliedState>,
    pub context: ManagedPolicyContext,
    pub egress: crate::types::egress::SessionEgressPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProxyState {
    pub incarnation: String,
    pub session_id: crate::SessionId,
    pub sandbox_id: crate::SandboxId,
    pub guest_ip: std::net::Ipv4Addr,
    pub policy: ManagedDenyPolicy,
}

/// Retain the policy in its existing JSON document, including unrelated fields.
pub fn retain_in_policy_json(
    old: Option<&str>,
    next: &ManagedDenyPolicy,
) -> Result<String, String> {
    next.validate()?;
    let mut value: serde_json::Value = match old {
        Some(s) => serde_json::from_str(s).map_err(|e| e.to_string())?,
        None => serde_json::json!({}),
    };
    let object = value.as_object_mut().ok_or("invalid integration policy")?;
    if let Some(old) = object.get("managed_deny").filter(|v| !v.is_null()) {
        let old: ManagedDenyPolicy =
            serde_json::from_value(old.clone()).map_err(|e| e.to_string())?;
        old.validate()?;
        if next.revision < old.revision
            || !next.contains(&old)
            || (next.revision == old.revision && next != &old)
        {
            return Err("managed policy regressed".into());
        }
    }
    object.insert(
        "managed_deny".into(),
        serde_json::to_value(next).map_err(|e| e.to_string())?,
    );
    serde_json::to_string(&value).map_err(|e| e.to_string())
}
pub fn validate_policy_replacement(old: Option<&str>, next: &str) -> Result<(), String> {
    let old: serde_json::Value = old
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(serde_json::Value::Null);
    if old.get("managed_deny").is_some_and(|v| !v.is_null()) {
        let new: serde_json::Value = serde_json::from_str(next).map_err(|e| e.to_string())?;
        let policy = serde_json::from_value(
            new.get("managed_deny")
                .ok_or("managed policy cannot be removed")?
                .clone(),
        )
        .map_err(|e| e.to_string())?;
        retain_in_policy_json(Some(&old.to_string()), &policy)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_digest_and_exact_authorities_match_the_control_contract() {
        let policy = ManagedDenyPolicy::new(
            7,
            ["MCP.EXAMPLE.", "alias.example", "mcp.example"].map(String::from),
        )
        .unwrap();
        assert_eq!(
            policy.digest,
            "sha256:e8052caef31fb8d39292ff014cf58f711b253b326645e2066b5210b3bb49d265"
        );
        assert_eq!(policy.hosts, ["alias.example", "mcp.example"]);
        assert!(policy.denies("MCP.EXAMPLE."));
        for other in [
            "other.mcp.example",
            "mcp.example.other",
            "127.0.0.1",
            "mcp.example:443",
        ] {
            assert!(!policy.denies(other));
        }
        for invalid in [
            "127.0.0.1",
            "::1",
            "localhost",
            "*.example",
            "a..example",
            "mcp.example..",
            "mcp.example:443",
            "mcp.example/",
            "mcp.example\n",
        ] {
            assert!(ManagedDenyPolicy::new(1, [invalid.into()]).is_err());
        }
        let mut reordered = policy.clone();
        reordered.hosts.reverse();
        assert!(reordered.validate().is_err());
        let mut changed = policy;
        changed.revision += 1;
        assert!(changed.validate().is_err());
    }
}
