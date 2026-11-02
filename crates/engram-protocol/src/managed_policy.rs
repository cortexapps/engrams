//! Strict conversions for the dedicated managed policy control service.
use crate::mcp as wire;
use engram_core::types::managed_deny as core;
fn missing() -> String {
    "required managed policy field absent".into()
}
impl TryFrom<wire::ManagedPolicyTarget> for core::ManagedPolicyTarget {
    type Error = String;
    fn try_from(v: wire::ManagedPolicyTarget) -> Result<Self, String> {
        let target = Self {
            session_id: v
                .session_id
                .parse()
                .map_err(|_| "invalid session identity")?,
            sandbox_id: v
                .sandbox_id
                .parse()
                .map_err(|_| "invalid sandbox identity")?,
            host_id: v.host_id.parse().map_err(|_| "invalid host identity")?,
            binding_epoch: v.binding_epoch,
        };
        if target.session_id.as_uuid().is_nil()
            || target.sandbox_id.as_uuid().is_nil()
            || target.host_id.as_uuid().is_nil()
            || target.binding_epoch == 0
            || target.binding_epoch > 9_007_199_254_740_991
        {
            return Err("invalid physical placement".into());
        }
        Ok(target)
    }
}
impl From<core::ManagedPolicyTarget> for wire::ManagedPolicyTarget {
    fn from(v: core::ManagedPolicyTarget) -> Self {
        Self {
            session_id: v.session_id.to_string(),
            sandbox_id: v.sandbox_id.to_string(),
            host_id: v.host_id.to_string(),
            binding_epoch: v.binding_epoch,
        }
    }
}
impl TryFrom<wire::ManagedPolicyContext> for core::ManagedPolicyContext {
    type Error = String;
    fn try_from(v: wire::ManagedPolicyContext) -> Result<Self, String> {
        match (wire::ManagedPolicyPhase::try_from(v.phase), &v.binding_hash) {
            (Ok(wire::ManagedPolicyPhase::Bootstrap), None)
            | (Ok(wire::ManagedPolicyPhase::Final), Some(_)) => {}
            _ => return Err("managed policy phase and binding hash disagree".into()),
        }
        let value = Self {
            target: v.target.ok_or_else(missing)?.try_into()?,
            operator_generation: v.operator_generation,
            operator_identity: v.operator_identity,
            binding_hash: v.binding_hash,
        };
        value.validate()?;
        Ok(value)
    }
}
impl From<core::ManagedPolicyContext> for wire::ManagedPolicyContext {
    fn from(v: core::ManagedPolicyContext) -> Self {
        Self {
            phase: if v.binding_hash.is_some() {
                wire::ManagedPolicyPhase::Final as i32
            } else {
                wire::ManagedPolicyPhase::Bootstrap as i32
            },
            target: Some(v.target.into()),
            operator_generation: v.operator_generation,
            operator_identity: v.operator_identity,
            binding_hash: v.binding_hash,
        }
    }
}
impl TryFrom<wire::ManagedDenyPolicy> for core::ManagedDenyPolicy {
    type Error = String;
    fn try_from(v: wire::ManagedDenyPolicy) -> Result<Self, String> {
        let value = Self {
            version: v.version,
            revision: v.revision,
            hosts: v.hosts,
            digest: v.digest,
        };
        value.validate()?;
        Ok(value)
    }
}
impl From<core::ManagedDenyPolicy> for wire::ManagedDenyPolicy {
    fn from(v: core::ManagedDenyPolicy) -> Self {
        Self {
            version: v.version,
            revision: v.revision,
            hosts: v.hosts,
            digest: v.digest,
        }
    }
}
impl TryFrom<wire::ManagedAppliedState> for core::ManagedAppliedState {
    type Error = String;
    fn try_from(v: wire::ManagedAppliedState) -> Result<Self, String> {
        let value = Self {
            context: v.context.ok_or_else(missing)?.try_into()?,
            policy: v.policy.ok_or_else(missing)?.try_into()?,
            host_incarnation: v.host_incarnation,
            proxy_incarnation: v.proxy_incarnation,
            state_id: v.state_id,
            network_proof: v.network_proof.map(TryInto::try_into).transpose()?,
        };
        value.validate()?;
        Ok(value)
    }
}
impl From<core::ManagedAppliedState> for wire::ManagedAppliedState {
    fn from(v: core::ManagedAppliedState) -> Self {
        Self {
            context: Some(v.context.into()),
            policy: Some(v.policy.into()),
            host_incarnation: v.host_incarnation,
            proxy_incarnation: v.proxy_incarnation,
            state_id: v.state_id,
            network_proof: v.network_proof.map(Into::into),
        }
    }
}
impl TryFrom<wire::ManagedNetworkProof> for core::ManagedNetworkProof {
    type Error = String;
    fn try_from(v: wire::ManagedNetworkProof) -> Result<Self, String> {
        Ok(Self {
            target: v.target.ok_or_else(missing)?.try_into()?,
            guest_ip: v.guest_ip.parse().map_err(|_| "invalid guest identity")?,
            network_identity: v.network_identity,
        })
    }
}
impl From<core::ManagedNetworkProof> for wire::ManagedNetworkProof {
    fn from(v: core::ManagedNetworkProof) -> Self {
        Self {
            target: Some(v.target.into()),
            guest_ip: v.guest_ip.to_string(),
            network_identity: v.network_identity,
        }
    }
}

/// The managed bincode surface is bounded and rejects unknown trailing fields.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, bincode::Error> {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(4 * 1024 * 1024)
        .reject_trailing_bytes()
        .deserialize(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_phase_and_false_final_ack_are_closed() {
        for (phase, binding_hash) in [
            (0, None),
            (999, None),
            (2, None),
            (1, Some("sha256:invalid".into())),
            (2, Some(String::new())),
        ] {
            assert!(
                core::ManagedPolicyContext::try_from(wire::ManagedPolicyContext {
                    phase,
                    binding_hash,
                    ..Default::default()
                })
                .is_err()
            );
        }
    }
}
