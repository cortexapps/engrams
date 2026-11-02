//! Configured operator authority and the durable all-session publication barrier.
use super::{
    managed_boot::{
        ManagedBootPrepare, ManagedBootPreparedReceipt, ManagedBootRetiredReceipt,
        ManagedCaptureAuthority,
    },
    managed_create::digest,
    managed_deny::{canonical_host, is_digest, ManagedDenyPolicy, ManagedPolicyTarget},
};
use crate::{HostId, MetaError, SandboxId, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorZone {
    pub revision: String,
    /// The worker's configuration digest is not a network or trust attestation.
    pub worker_digest: String,
    pub authorities: Vec<String>,
    pub aliases: Vec<String>,
    /// Exact operator configuration, including routes, CIDRs, limits and trust references.
    pub configuration: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCandidate {
    pub version: u32,
    pub zones: BTreeMap<String, OperatorZone>,
    pub deployment_identity: String,
    pub network_identity: String,
    pub trust_identity: String,
    /// Digests of actual CA contents supplied by the trusted projection producer.
    pub ca_content_digests: BTreeMap<String, String>,
}
impl OperatorCandidate {
    pub fn hosts(&self) -> Result<Vec<String>, MetaError> {
        if self.version != 1
            || self.zones.is_empty()
            || self.zones.len() > 128
            || ![
                &self.deployment_identity,
                &self.network_identity,
                &self.trust_identity,
            ]
            .into_iter()
            .all(|s| is_digest(s))
            || self.ca_content_digests.is_empty()
            || self
                .ca_content_digests
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 128 || !is_digest(v))
        {
            return Err(conflict());
        }
        let mut hosts = BTreeSet::new();
        for (id, zone) in &self.zones {
            if id.is_empty()
                || id.len() > 128
                || zone.revision.is_empty()
                || zone.revision.len() > 128
                || !is_digest(&zone.worker_digest)
                || !zone.configuration.is_object()
                || zone.authorities.is_empty()
                || !sorted(&zone.authorities)
                || !sorted(&zone.aliases)
            {
                return Err(conflict());
            }
            let config = &zone.configuration;
            let allowed = [
                "id",
                "revision",
                "mode",
                "authorities",
                "aliases",
                "cidrs",
                "denyCidrs",
                "ca",
                "limits",
            ];
            if config
                .as_object()
                .is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
                || !matches!(
                    config.get("mode").and_then(|v| v.as_str()),
                    Some("public" | "internal")
                )
            {
                return Err(conflict());
            }
            for field in ["cidrs", "denyCidrs"] {
                let values = config
                    .get(field)
                    .and_then(|v| v.as_array())
                    .ok_or_else(conflict)?;
                for value in values {
                    let (ip, bits) = value
                        .as_str()
                        .and_then(|v| v.split_once('/'))
                        .ok_or_else(conflict)?;
                    let ip: std::net::IpAddr = ip.parse().map_err(|_| conflict())?;
                    let bits: u8 = bits.parse().map_err(|_| conflict())?;
                    if bits > if ip.is_ipv4() { 32 } else { 128 } {
                        return Err(conflict());
                    }
                }
            }
            if let Some(ca) = config.get("ca") {
                let ca = ca.as_str().ok_or_else(conflict)?;
                if !ca.starts_with("-----BEGIN CERTIFICATE-----")
                    || ca.contains("PRIVATE KEY")
                    || !self
                        .ca_content_digests
                        .values()
                        .any(|d| *d == digest(ca.as_bytes()))
                {
                    return Err(conflict());
                }
            }
            if let Some(limits) = config.get("limits") {
                let limits = limits.as_object().ok_or_else(conflict)?;
                if limits.iter().any(|(k, v)| {
                    !["execute", "discover", "oauth", "schema", "toolMs"].contains(&k.as_str())
                        || v.as_u64()
                            .is_none_or(|n| n == 0 || n > 9_007_199_254_740_991)
                }) {
                    return Err(conflict());
                }
            }
            let projected = |field: &str| -> Option<Vec<String>> {
                let mut values: Vec<String> = config
                    .get(field)?
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()?;
                values.sort();
                values.dedup();
                Some(values)
            };
            if config.get("id").and_then(|v| v.as_str()) != Some(id.as_str())
                || config.get("revision").and_then(|v| v.as_str()) != Some(zone.revision.as_str())
                || projected("authorities").as_ref() != Some(&zone.authorities)
                || projected("aliases").as_ref() != Some(&zone.aliases)
            {
                return Err(conflict());
            }
            for authority in &zone.authorities {
                let (host, port) = match authority.split_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (authority.as_str(), None),
                };
                if canonical_host(host).as_deref() != Some(host)
                    || port.is_some_and(|p| {
                        p.parse::<u16>()
                            .ok()
                            .filter(|v| *v != 0 && *v != 443)
                            .is_none_or(|v| v.to_string() != p)
                    })
                {
                    return Err(conflict());
                }
                hosts.insert(host.to_owned());
            }
            for alias in &zone.aliases {
                if canonical_host(alias).as_ref() != Some(alias) {
                    return Err(conflict());
                }
                hosts.insert(alias.clone());
            }
        }
        if hosts.len() > 4096
            || serde_json::to_vec(self).map_err(|_| conflict())?.len() > 1024 * 1024
        {
            return Err(conflict());
        }
        Ok(hosts.into_iter().collect())
    }
    pub fn identity(&self) -> Result<String, MetaError> {
        self.hosts()?;
        Ok(digest(&serde_json::to_vec(self).map_err(|_| conflict())?))
    }
}
fn sorted(v: &[String]) -> bool {
    v.windows(2).all(|p| p[0] < p[1])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorGeneration {
    pub generation: u64,
    pub expected_generation: Option<u64>,
    pub identity: String,
    pub candidate: OperatorCandidate,
}
impl OperatorGeneration {
    pub fn new(expected: Option<u64>, candidate: OperatorCandidate) -> Result<Self, MetaError> {
        let generation = expected
            .unwrap_or(0)
            .checked_add(1)
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or_else(conflict)?;
        let identity = candidate.identity()?;
        Ok(Self {
            generation,
            expected_generation: expected,
            identity,
            candidate,
        })
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPublication {
    pub published: Option<OperatorGeneration>,
    pub pending: Option<OperatorGeneration>,
    pub admission_open: bool,
}
impl OperatorPublication {
    /// A new legacy marker must include the published set before it can register.
    pub fn validate_registration(&self, old: Option<&str>, next: &str) -> Result<(), MetaError> {
        let managed = |raw: Option<&str>| -> Result<bool, MetaError> {
            Ok(raw
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .map_err(|_| conflict())?
                .is_some_and(|v| v.get("managed_deny").is_some_and(|p| !p.is_null())))
        };
        if !managed(old)? && managed(Some(next))? {
            if self.pending.is_some() {
                return Err(conflict());
            }
            if let Some(published) = &self.published {
                if !self.admission_open {
                    return Err(conflict());
                }
                let (_, policy) = retain(Some(next), Vec::new())?;
                if !published
                    .candidate
                    .hosts()?
                    .iter()
                    .all(|h| policy.hosts.contains(h))
                {
                    return Err(conflict());
                }
            }
        }
        Ok(())
    }

    pub fn stage(
        &self,
        expected: Option<u64>,
        candidate: OperatorCandidate,
    ) -> Result<Self, MetaError> {
        let next = OperatorGeneration::new(expected, candidate)?;
        if self.pending.as_ref() == Some(&next)
            || (self.pending.is_none() && self.published.as_ref() == Some(&next))
        {
            return Ok(self.clone());
        }
        if self.pending.is_some() || self.published.as_ref().map(|p| p.generation) != expected {
            return Err(conflict());
        }
        Ok(Self {
            published: self.published.clone(),
            pending: Some(next),
            admission_open: false,
        })
    }
    pub fn pending(&self, generation: &OperatorGeneration) -> Result<(), MetaError> {
        if self.admission_open || self.pending.as_ref() != Some(generation) {
            return Err(conflict());
        }
        Ok(())
    }
}

/// A snapshot of durable placement, registered attempt and next-boot policy.
/// Terminal UI status is included for CAS, never interpreted as destruction proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorParticipant {
    pub session_id: SessionId,
    pub status: String,
    pub host_id: Option<HostId>,
    pub sandbox_id: Option<SandboxId>,
    pub binding_epoch: u64,
    pub held_creation: bool,
    pub boot: Option<(ManagedBootPrepare, u64, Option<ManagedBootPreparedReceipt>)>,
    pub policy: ManagedDenyPolicy,
}
impl OperatorParticipant {
    pub fn target(&self) -> Result<Option<ManagedPolicyTarget>, MetaError> {
        match (self.host_id, self.sandbox_id) {
            (Some(host_id), Some(sandbox_id)) => {
                let target = ManagedPolicyTarget {
                    session_id: self.session_id,
                    host_id,
                    sandbox_id,
                    binding_epoch: self.binding_epoch,
                };
                if self
                    .boot
                    .as_ref()
                    .is_some_and(|(k, _, _)| k.key.context.target != target)
                {
                    return Err(conflict());
                }
                Ok(Some(target))
            }
            (host, None) => {
                let Some((request, _, _)) = &self.boot else {
                    return if host.is_none() {
                        Ok(None)
                    } else {
                        Err(conflict())
                    };
                };
                let target = &request.key.context.target;
                if target.session_id != self.session_id
                    || target.binding_epoch != self.binding_epoch
                    || host.is_some_and(|h| h != target.host_id)
                {
                    return Err(conflict());
                }
                Ok(Some(target.clone()))
            }
            _ => Err(conflict()),
        }
    }
    pub fn dormant(&self) -> bool {
        self.held_creation
            && self.boot.is_none()
            && self.binding_epoch == 0
            && self.host_id.is_none()
            && self.sandbox_id.is_none()
            && self.status == "queued"
    }
}
/// Publication accepts only never-allocated or permanent source evidence.
/// Live Applied receipts cannot represent durably invalidated service lifetimes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperatorReadiness {
    NeverAllocated,
    Retired(Box<ManagedBootRetiredReceipt>),
    Captured(Box<ManagedCaptureAuthority>),
}
impl OperatorReadiness {
    pub fn validate(
        &self,
        generation: &OperatorGeneration,
        p: &OperatorParticipant,
    ) -> Result<(), MetaError> {
        p.policy.validate().map_err(MetaError::Conflict)?;
        if !generation
            .candidate
            .hosts()?
            .iter()
            .all(|h| p.policy.hosts.contains(h))
        {
            return Err(conflict());
        }
        match self {
            Self::NeverAllocated if p.dormant() => Ok(()),
            Self::Captured(a) => {
                a.validate()?;
                let (request, _, prepared) = p.boot.as_ref().ok_or_else(conflict)?;
                let key = &request.key;
                if a.request.key != *key
                    || a.image != request.cold.image
                    || a.kernel_sha256 != request.cold.kernel_sha256
                    || a.aux_ro_drives != request.cold.aux_ro_drives
                    || Some(&key.context.target) != p.target()?.as_ref()
                    || prepared
                        .as_ref()
                        .is_some_and(|r| r.host_incarnation != a.host_incarnation)
                {
                    return Err(conflict());
                }
                Ok(())
            }
            Self::Retired(r) => {
                let (request, _, prepared) = p.boot.as_ref().ok_or_else(conflict)?;
                let key = &request.key;
                if r.prepared.request != *request
                    || Some(&key.context.target) != p.target()?.as_ref()
                {
                    return Err(conflict());
                }
                r.validate_for(
                    prepared.as_ref().unwrap_or(&r.prepared),
                    &request.bootstrap_policy,
                )?;
                if !p.policy.contains(&r.retained_policy)
                    || p.policy.revision < r.retained_policy.revision
                {
                    return Err(conflict());
                }
                Ok(())
            }
            _ => Err(conflict()),
        }
    }
}
/// Union history in the existing IntegrationPolicy. Repeated scans keep a stable revision.
pub fn retain(
    raw: Option<&str>,
    hosts: Vec<String>,
) -> Result<(String, ManagedDenyPolicy), MetaError> {
    let value: serde_json::Value = raw
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| conflict())?
        .unwrap_or(serde_json::json!({}));
    let old: Option<ManagedDenyPolicy> = value
        .get("managed_deny")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|_| conflict())?;
    if let Some(old) = &old {
        old.validate().map_err(MetaError::Conflict)?;
    }
    let all = hosts
        .into_iter()
        .chain(old.iter().flat_map(|p| p.hosts.iter().cloned()));
    let mut policy = ManagedDenyPolicy::new(old.as_ref().map_or(1, |p| p.revision), all)
        .map_err(MetaError::Conflict)?;
    if old.as_ref().is_some_and(|o| o.hosts != policy.hosts) {
        policy = ManagedDenyPolicy::new(
            policy.revision.checked_add(1).ok_or_else(conflict)?,
            policy.hosts,
        )
        .map_err(MetaError::Conflict)?;
    }
    Ok((
        super::managed_deny::retain_in_policy_json(raw, &policy).map_err(MetaError::Conflict)?,
        policy,
    ))
}
pub fn conflict() -> MetaError {
    MetaError::Conflict("operator publication changed or unavailable".into())
}

/// A new operator candidate invalidates old receipts even if its host set is unchanged.
pub fn retain_generation(
    raw: Option<&str>,
    hosts: Vec<String>,
    first: bool,
) -> Result<(String, ManagedDenyPolicy), MetaError> {
    let (mut json, mut policy) = retain(raw, hosts)?;
    if first && raw.is_some() {
        let (_, old) = retain(raw, Vec::new())?;
        if policy.revision == old.revision {
            policy = ManagedDenyPolicy::new(
                policy.revision.checked_add(1).ok_or_else(conflict)?,
                policy.hosts,
            )
            .map_err(MetaError::Conflict)?;
            json = super::managed_deny::retain_in_policy_json(raw, &policy)
                .map_err(MetaError::Conflict)?;
        }
    }
    Ok((json, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate() -> OperatorCandidate {
        let zone = OperatorZone {
            revision: "1".into(),
            worker_digest: digest(b"worker"),
            authorities: vec!["unused.example:8443".into()],
            aliases: vec!["alias.example".into()],
            configuration: serde_json::json!({"id":"unused","revision":"1","mode":"public","authorities":["unused.example:8443"],"aliases":["alias.example"],"cidrs":[],"denyCidrs":[]}),
        };
        OperatorCandidate {
            version: 1,
            zones: [("unused".into(), zone)].into(),
            deployment_identity: digest(b"deployment"),
            network_identity: digest(b"network"),
            trust_identity: digest(b"trust"),
            ca_content_digests: [("root".into(), digest(b"certificate"))].into(),
        }
    }
    #[test]
    fn complete_projection_and_non_host_changes_have_distinct_identity() {
        let original = candidate();
        assert_eq!(
            original.hosts().unwrap(),
            ["alias.example", "unused.example"]
        );
        for field in ["cidrs", "denyCidrs"] {
            let mut changed = original.clone();
            changed.zones.get_mut("unused").unwrap().configuration[field] =
                serde_json::json!(["192.0.2.0/24"]);
            assert_eq!(changed.hosts().unwrap(), original.hosts().unwrap());
            assert_ne!(changed.identity().unwrap(), original.identity().unwrap());
        }
        let mut changed = original.clone();
        changed.trust_identity = digest(b"next trust");
        assert_ne!(changed.identity().unwrap(), original.identity().unwrap());
    }
    #[test]
    fn candidate_rejects_credentials_invalid_origins_and_projection_drift() {
        let original = candidate();
        for (field, value) in [
            ("credentials", serde_json::json!({"token":"fixture"})),
            ("ca", serde_json::json!("-----BEGIN PRIVATE KEY-----")),
            ("cidrs", serde_json::json!(["192.0.2.0/99"])),
            ("limits", serde_json::json!({"token":"fixture"})),
            ("authorities", serde_json::json!(["another.example"])),
        ] {
            let mut invalid = original.clone();
            invalid.zones.get_mut("unused").unwrap().configuration[field] = value;
            assert!(invalid.identity().is_err(), "{field}");
        }
        for authority in [
            "UPPER.example",
            "https://api.example",
            "api.example:443",
            "api.example:0",
            "api.example:0444",
            "127.0.0.1",
        ] {
            let mut invalid = original.clone();
            let zone = invalid.zones.get_mut("unused").unwrap();
            zone.authorities = vec![authority.into()];
            zone.configuration["authorities"] = serde_json::json!([authority]);
            assert!(invalid.identity().is_err(), "{authority}");
        }
        let mut invalid = original;
        invalid.ca_content_digests.clear();
        assert!(invalid.identity().is_err());
    }
    #[test]
    fn ca_identity_uses_actual_content_not_a_path() {
        let mut value = candidate();
        let ca = "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----";
        value.zones.get_mut("unused").unwrap().configuration["ca"] = ca.into();
        assert!(value.identity().is_err());
        value
            .ca_content_digests
            .insert("root".into(), digest(ca.as_bytes()));
        assert!(value.identity().is_ok());
    }
}
