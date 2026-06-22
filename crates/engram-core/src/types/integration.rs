//! ADR 0056: the compiled, per-session integration policy (option B′).
//!
//! The orchestrator compiles a profile's bound capabilities against its
//! connector config into this policy and ships it on `CreateSession` as a
//! JSON string (an orchestrator-authored artifact, opaque to the proto
//! layer — like an event `payload_json`). The coordinator persists it
//! (resume-safe) and resolves each `secret_ref` host-side, via its
//! `SecretStore`, into the egress policy the host proxy enforces. Secret
//! *refs* cross the wire from the orchestrator; secret *values* never leave
//! the coordinator/host.
//!
//! This phase carries Plane-B injections (gate + inject). Phase 4 adds asset
//! specs; Phase 5 adds mint scopes.

use serde::{Deserialize, Serialize};

/// The per-session compiled policy. Persisted as JSON keyed by session;
/// re-read on resume to rebuild the egress policy without the orchestrator.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationPolicy {
    /// Plane-B credential injections selected by the session's capabilities.
    #[serde(default)]
    pub injects: Vec<IntegrationInject>,
}

/// One Plane-B injection: on an outbound request to `hosts` matching the
/// request policy (`methods` + `path_prefixes`), the proxy injects
/// `header_name: <header_template with "{}" → the resolved secret>`. The
/// `secret_ref` is resolved by the coordinator's `SecretStore` host-side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInject {
    pub hosts: Vec<String>,
    pub header_name: String,
    /// `{}` is replaced by the resolved secret (e.g. `"Bearer {}"`, `"{}"`).
    pub header_template: String,
    /// A `SecretStore` reference (e.g. `"datadog-api-key"`) the coordinator
    /// resolves to a value host-side. NEVER a secret value itself.
    pub secret_ref: String,
    /// Request shapes this injection gates + applies to. Empty = any.
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path_prefixes: Vec<String>,
}

impl IntegrationPolicy {
    /// Parse the JSON the orchestrator ships on `CreateSession`. An empty /
    /// whitespace-only string is an absent policy (`None`).
    pub fn parse(json: &str) -> Result<Option<Self>, serde_json::Error> {
        if json.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(json).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_parses_to_none() {
        assert_eq!(IntegrationPolicy::parse("").unwrap(), None);
        assert_eq!(IntegrationPolicy::parse("   ").unwrap(), None);
    }

    #[test]
    fn round_trips_an_inject() {
        let p = IntegrationPolicy {
            injects: vec![IntegrationInject {
                hosts: vec!["api.datadoghq.com".into()],
                header_name: "DD-API-KEY".into(),
                header_template: "{}".into(),
                secret_ref: "datadog-api-key".into(),
                methods: vec!["GET".into()],
                path_prefixes: vec!["/api/v2/logs".into()],
            }],
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(IntegrationPolicy::parse(&json).unwrap(), Some(p));
    }

    #[test]
    fn defaults_fill_missing_request_policy() {
        // A connector that gates only by host omits methods/path_prefixes.
        let json = r#"{"injects":[{"hosts":["h"],"header_name":"X","header_template":"{}","secret_ref":"r"}]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert!(p.injects[0].methods.is_empty());
        assert!(p.injects[0].path_prefixes.is_empty());
    }
}
