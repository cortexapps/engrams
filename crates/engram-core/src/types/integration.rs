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
///
/// ADR 0057: this is now the full **session policy** — the orchestrator compiles
/// the whole profile (network + secrets + integration capabilities) into it, and
/// the coordinator sources the egress policy's network + secrets from here
/// instead of the image manifest. The type keeps its `IntegrationPolicy` name and
/// the `integration_policy_json` wire field for now; the cosmetic rename to
/// `SessionPolicy` is a deferred follow-up (cf. ADR 0056's deferred
/// `ForgeOp`→`IntegrationOp` rename). All new fields are `#[serde(default)]`, so
/// policies persisted before 0057 still parse.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationPolicy {
    /// Plane-B credential injections selected by the session's capabilities.
    #[serde(default)]
    pub injects: Vec<IntegrationInject>,
    /// ADR 0056 Phase 4: response-observation specs — the connector `asset`
    /// maps for the session's capabilities. The proxy emits an
    /// `IntegrationAsset` from the real response of a matching request. Carries
    /// no secret (the asset map is pure), so the coordinator copies these
    /// straight through to the egress policy (no host-side resolution).
    #[serde(default)]
    pub observes: Vec<IntegrationObserve>,
    /// ADR 0057: the profile's egress network allow-list (deny by default),
    /// lifted off the image manifest. The coordinator builds the session egress
    /// policy's `network_allow_*` from this.
    #[serde(default)]
    pub network: crate::types::image::NetworkPolicy,
    /// ADR 0057: profile-defined secrets injected into the session, lifted off
    /// the image manifest. Each `secret_ref` is resolved host-side via the
    /// coordinator's `SecretStore` (the org-secret backend); a `literal` secret
    /// is placed in the guest env, a `broker` secret becomes an env placeholder
    /// the proxy substitutes only on its `allow_hosts`.
    #[serde(default)]
    pub secrets: Vec<IntegrationSecret>,
}

/// ADR 0057: one profile-defined secret the session injects. The value lives in
/// the org secret store (resolved by the coordinator from `secret_ref`); this
/// carries only the ref + how to inject it. NEVER a secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSecret {
    /// `SecretStore` reference (the org-secret name). NEVER a value.
    pub secret_ref: String,
    /// Env var the resolved value is exposed as in the guest.
    pub env_var: String,
    /// `literal` (raw value in the guest env) | `broker` (env placeholder +
    /// proxy substitution only on `allow_hosts`; the guest never holds it).
    #[serde(default)]
    pub mode: crate::types::image::SecretMode,
    /// Broker-mode substitution hosts (exact).
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// Broker-mode substitution host globs (`*.example.com`).
    #[serde(default)]
    pub allow_host_patterns: Vec<String>,
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
    /// resolves to a value host-side. NEVER a secret value itself. Empty when
    /// this is a `mint_provider` entry (the value is minted, not stored).
    #[serde(default)]
    pub secret_ref: String,
    /// ADR 0056 amendment: when non-empty, the inject value is **minted** — the
    /// coordinator resolves it via the `IntegrationBroker` for this provider,
    /// scoped to the session's bound capabilities, instead of from `secret_ref`.
    /// This is how a *mint* provider (e.g. github) rides the same egress inject
    /// plane as a static-secret (*inject*) provider; the scoped token never
    /// enters the guest. Mutually exclusive with `secret_ref`. NEVER a value.
    #[serde(default)]
    pub mint_provider: String,
    /// Request shapes this injection gates + applies to. Empty = any.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Glob patterns (a `*` matches any run of chars, incl. `/`) matched against
    /// the whole request path — e.g. `/repos/*/pulls`. Empty = any path. The name
    /// is historical: these used to be literal prefixes (see the egress proxy's
    /// `RequestPolicy`). Carries the connector op's full `match.path`.
    #[serde(default)]
    pub path_prefixes: Vec<String>,
}

/// One response-observation spec. On an outbound request to `hosts` matching
/// `methods` + `path_prefixes` (path globs — see `IntegrationInject`), the proxy
/// parses the response and emits an
/// `IntegrationAsset` (`provider`/`asset_kind`/`surface`) built from the `data`
/// extractor map + optional `fetchable` URL extractor, gated by `success`.
/// Extractor paths are `$.resp.<dotted>` / `$.req.method|path` / `$.status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationObserve {
    pub hosts: Vec<String>,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path_prefixes: Vec<String>,
    pub provider: String,
    pub asset_kind: String,
    /// `"action"` (transient) | `"asset"` (durable). Opaque to the proxy; the
    /// coordinator maps it to its `AssetSurface`.
    pub surface: String,
    /// Status class gating emission, e.g. `"2xx"`. `None` → emit on any status.
    #[serde(default)]
    pub success_status_class: Option<String>,
    /// `(field name, extractor path)` pairs for the asset's `data` payload.
    #[serde(default)]
    pub data: Vec<(String, String)>,
    /// Extractor path producing an external URL, e.g. `"$.resp.html_url"`.
    #[serde(default)]
    pub fetchable: Option<String>,
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
                mint_provider: String::new(),
                methods: vec!["GET".into()],
                path_prefixes: vec!["/api/v2/logs*".into()],
            }],
            observes: vec![],
            ..Default::default()
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

    #[test]
    fn round_trips_an_observe() {
        let p = IntegrationPolicy {
            injects: vec![],
            observes: vec![IntegrationObserve {
                hosts: vec!["api.github.com".into()],
                methods: vec!["POST".into()],
                path_prefixes: vec!["/repos/*/issues".into()],
                provider: "github".into(),
                asset_kind: "issue".into(),
                surface: "asset".into(),
                success_status_class: Some("2xx".into()),
                data: vec![("number".into(), "$.resp.number".into())],
                fetchable: Some("$.resp.html_url".into()),
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(IntegrationPolicy::parse(&json).unwrap(), Some(p));
    }

    #[test]
    fn observes_default_to_empty_when_absent() {
        // A policy serialized before Phase 4 (injects only) still decodes.
        let json = r#"{"injects":[]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert!(p.observes.is_empty());
    }

    #[test]
    fn round_trips_network_and_secrets() {
        // ADR 0057: the policy now carries the profile's network + secrets.
        use crate::types::image::{NetworkDefault, NetworkPolicy, SecretMode};
        let p = IntegrationPolicy {
            network: NetworkPolicy {
                default: NetworkDefault::Deny,
                allow_hosts: vec!["sentry.io".into()],
                allow_host_patterns: vec!["*.pypi.org".into()],
            },
            secrets: vec![IntegrationSecret {
                secret_ref: "datadog-api-key".into(),
                env_var: "DD_API_KEY".into(),
                mode: SecretMode::Broker,
                allow_hosts: vec!["api.datadoghq.com".into()],
                allow_host_patterns: vec![],
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(IntegrationPolicy::parse(&json).unwrap(), Some(p));
    }

    #[test]
    fn network_and_secrets_default_when_absent() {
        // A policy serialized before 0057 (no network/secrets) still decodes:
        // network defaults to deny+empty, secrets to empty.
        let json = r#"{"injects":[],"observes":[]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert!(p.secrets.is_empty());
        assert!(p.network.allow_hosts.is_empty());
        assert_eq!(p.network.default, crate::types::image::NetworkDefault::Deny);
    }
}
