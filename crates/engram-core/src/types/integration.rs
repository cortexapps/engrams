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
/// request policy (`methods` + `path_globs`), the proxy injects
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
    /// the whole request path — e.g. `/repos/*/pulls`. Empty = any path. Carries
    /// the connector op's full `match.path`; matched by the egress proxy's
    /// `RequestPolicy`.
    #[serde(default)]
    pub path_globs: Vec<String>,
    /// ADR 0059: GraphQL operation type for body-parsed gating (`"query"` |
    /// `"mutation"` | `"subscription"`). Empty = a REST entry (gate by
    /// methods/path only). Paired with `graphql_field`; for a GraphQL entry
    /// `path_globs` carries the `/graphql` endpoint and `methods` is `["POST"]`,
    /// so it is *also* method+path gated (defense in depth).
    #[serde(default)]
    pub graphql_operation: String,
    /// ADR 0059: the GraphQL top-level field this entry authorizes (e.g.
    /// `"mergePullRequest"`). Empty = a REST entry.
    #[serde(default)]
    pub graphql_field: String,
}

/// One response-observation spec. On an outbound request to `hosts` matching
/// `methods` + `path_globs` (path globs — see `IntegrationInject`), the proxy
/// parses the response and emits an
/// `IntegrationAsset` (`provider`/`asset_kind`/`surface`) built from the `data`
/// extractor map + optional `fetchable` URL extractor, gated by `success`.
/// Extractor paths are `$.resp.<dotted>` / `$.req.method|path` / `$.status` /
/// `$.vars.<dotted>` (the GraphQL request's `variables` — mutation inputs a
/// GraphQL response won't echo; `None`-valued on REST requests).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationObserve {
    pub hosts: Vec<String>,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path_globs: Vec<String>,
    pub provider: String,
    pub asset_kind: String,
    /// `"action"` (transient) | `"asset"` (durable). Opaque to the proxy; the
    /// coordinator maps it to its `AssetSurface`.
    pub surface: String,
    /// Status class gating emission, e.g. `"2xx"`. `None` → emit on any status.
    #[serde(default)]
    pub success_status_class: Option<String>,
    /// ADR 0059: GraphQL success rule — emit only when the response carries no
    /// non-empty top-level `errors` array (in addition to HTTP 2xx). Set for a
    /// GraphQL observe; takes precedence over `success_status_class`.
    #[serde(default)]
    pub success_no_graphql_errors: bool,
    /// ADR 0059: GraphQL operation type for body-parsed firing (`"query"` |
    /// `"mutation"`). Empty = a REST observe (fire by methods/path). Paired with
    /// `graphql_field`.
    #[serde(default)]
    pub graphql_operation: String,
    /// ADR 0059: the GraphQL top-level field this observe fires on (e.g.
    /// `"createIssue"`). Empty = a REST observe.
    #[serde(default)]
    pub graphql_field: String,
    /// `(field name, extractor path)` pairs for the asset's `data` payload.
    #[serde(default)]
    pub data: Vec<(String, String)>,
    /// Extractor path producing an external URL, e.g. `"$.resp.html_url"`.
    #[serde(default)]
    pub fetchable: Option<String>,
    /// Derive `data` fields the extractors missed from the extracted fetchable
    /// URL (GraphQL parity — a GraphQL response echoes only the client's
    /// selection set, but the URL is trusted response data). `#[serde(default)]`
    /// so pre-existing persisted policies decode without it.
    #[serde(default)]
    pub url_fallback: Option<ObserveUrlFallback>,
}

/// The URL-derived fallback of an [`IntegrationObserve`]: `pattern` is matched
/// against the whole fetchable URL (`{name}` captures a run of characters
/// excluding `/`/`?`/`#`; `{name:int}` additionally requires an integer and
/// emits a JSON number); `fields` are `(data field, template over captures)`
/// pairs that fill only fields the response extractors missed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveUrlFallback {
    pub pattern: String,
    pub fields: Vec<(String, String)>,
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

/// ADR 0063 addendum: concatenate the selected harness's declared egress
/// (`harness.toml [egress]`) into the session policy's network, so the harness
/// can always reach its own model API without every profile re-listing
/// LLM-provider hosts. Applied ONCE at session create, before the policy is
/// persisted — every later consumer (queued boot, resume, recovery) re-reads
/// the merged policy.
///
/// Semantics:
/// - empty egress → policy unchanged (harnesses that declare nothing opt out);
/// - allow-default network → unchanged (everything is already reachable);
/// - deny-default → append the harness hosts/patterns not already present;
/// - absent policy (deny-all session, e.g. a direct/CLI create) → synthesize a
///   minimal policy carrying just the harness egress.
pub fn merge_harness_egress(
    policy: Option<IntegrationPolicy>,
    egress: &crate::types::harness::HarnessEgress,
) -> Option<IntegrationPolicy> {
    if egress.is_empty() {
        return policy;
    }
    let mut policy = policy.unwrap_or_default();
    if matches!(
        policy.network.default,
        crate::types::image::NetworkDefault::Allow
    ) {
        return Some(policy);
    }
    for host in &egress.allow_hosts {
        if !policy.network.allow_hosts.contains(host) {
            policy.network.allow_hosts.push(host.clone());
        }
    }
    for pattern in &egress.allow_host_patterns {
        if !policy.network.allow_host_patterns.contains(pattern) {
            policy.network.allow_host_patterns.push(pattern.clone());
        }
    }
    Some(policy)
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
                path_globs: vec!["/api/v2/logs*".into()],
                graphql_operation: String::new(),
                graphql_field: String::new(),
            }],
            observes: vec![],
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(IntegrationPolicy::parse(&json).unwrap(), Some(p));
    }

    #[test]
    fn round_trips_a_graphql_inject_and_observe() {
        // ADR 0059: a GraphQL inject (mint, gated by operation+field) + its observe.
        let p = IntegrationPolicy {
            injects: vec![IntegrationInject {
                hosts: vec!["api.github.com".into()],
                header_name: String::new(),
                header_template: String::new(),
                secret_ref: String::new(),
                mint_provider: "github".into(),
                methods: vec!["POST".into()],
                path_globs: vec!["/graphql".into()],
                graphql_operation: "mutation".into(),
                graphql_field: "createIssue".into(),
            }],
            observes: vec![IntegrationObserve {
                hosts: vec!["api.github.com".into()],
                methods: vec!["POST".into()],
                path_globs: vec!["/graphql".into()],
                provider: "github".into(),
                asset_kind: "issue".into(),
                surface: "asset".into(),
                success_status_class: None,
                success_no_graphql_errors: true,
                graphql_operation: "mutation".into(),
                graphql_field: "createIssue".into(),
                data: vec![("id".into(), "$.resp.data.createIssue.issue.id".into())],
                fetchable: None,
                url_fallback: None,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(IntegrationPolicy::parse(&json).unwrap(), Some(p));
    }

    #[test]
    fn graphql_fields_default_when_absent() {
        // A policy serialized before ADR 0059 (no graphql fields) still decodes.
        let json = r#"{"injects":[{"hosts":["h"],"header_name":"X","header_template":"{}","secret_ref":"r","methods":["POST"],"path_globs":["/graphql"]}]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert!(p.injects[0].graphql_operation.is_empty());
        assert!(p.injects[0].graphql_field.is_empty());
    }

    #[test]
    fn defaults_fill_missing_request_policy() {
        // A connector that gates only by host omits methods/path_globs.
        let json = r#"{"injects":[{"hosts":["h"],"header_name":"X","header_template":"{}","secret_ref":"r"}]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert!(p.injects[0].methods.is_empty());
        assert!(p.injects[0].path_globs.is_empty());
    }

    #[test]
    fn round_trips_an_observe() {
        let p = IntegrationPolicy {
            injects: vec![],
            observes: vec![IntegrationObserve {
                hosts: vec!["api.github.com".into()],
                methods: vec!["POST".into()],
                path_globs: vec!["/repos/*/issues".into()],
                provider: "github".into(),
                asset_kind: "issue".into(),
                surface: "asset".into(),
                success_status_class: Some("2xx".into()),
                success_no_graphql_errors: false,
                graphql_operation: String::new(),
                graphql_field: String::new(),
                data: vec![("number".into(), "$.resp.number".into())],
                fetchable: Some("$.resp.html_url".into()),
                url_fallback: Some(ObserveUrlFallback {
                    pattern: "https://github.com/{owner}/{name}/issues/{number:int}".into(),
                    fields: vec![("number".into(), "{number}".into())],
                }),
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
    fn observe_url_fallback_defaults_when_absent() {
        // An observe persisted before the URL-fallback field (resume path) decodes.
        let json = r#"{"observes":[{"hosts":["h"],"provider":"github","asset_kind":"issue","surface":"asset"}]}"#;
        let p = IntegrationPolicy::parse(json).unwrap().unwrap();
        assert_eq!(p.observes[0].url_fallback, None);
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

    fn egress(hosts: &[&str], patterns: &[&str]) -> crate::types::harness::HarnessEgress {
        crate::types::harness::HarnessEgress {
            allow_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            allow_host_patterns: patterns.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn merge_harness_egress_appends_and_dedupes_on_deny() {
        let policy = IntegrationPolicy {
            network: crate::types::image::NetworkPolicy {
                default: crate::types::image::NetworkDefault::Deny,
                allow_hosts: vec!["github.com".into(), "api.anthropic.com".into()],
                allow_host_patterns: vec![],
            },
            ..Default::default()
        };
        let merged = merge_harness_egress(
            Some(policy),
            &egress(
                &["api.anthropic.com", "statsig.anthropic.com"],
                &["*.example.dev"],
            ),
        )
        .unwrap();
        assert_eq!(
            merged.network.allow_hosts,
            vec!["github.com", "api.anthropic.com", "statsig.anthropic.com"]
        );
        assert_eq!(merged.network.allow_host_patterns, vec!["*.example.dev"]);
    }

    #[test]
    fn merge_harness_egress_is_a_noop_on_allow_default_and_empty_egress() {
        let allow = IntegrationPolicy {
            network: crate::types::image::NetworkPolicy {
                default: crate::types::image::NetworkDefault::Allow,
                allow_hosts: vec![],
                allow_host_patterns: vec![],
            },
            ..Default::default()
        };
        let merged =
            merge_harness_egress(Some(allow.clone()), &egress(&["api.anthropic.com"], &[]))
                .unwrap();
        assert_eq!(merged, allow);

        // Empty egress leaves an absent policy absent.
        assert_eq!(merge_harness_egress(None, &egress(&[], &[])), None);
    }

    #[test]
    fn merge_harness_egress_synthesizes_a_policy_when_absent() {
        let merged = merge_harness_egress(None, &egress(&["api.anthropic.com"], &[])).unwrap();
        assert_eq!(
            merged.network.default,
            crate::types::image::NetworkDefault::Deny
        );
        assert_eq!(merged.network.allow_hosts, vec!["api.anthropic.com"]);
        assert!(merged.injects.is_empty());
        assert!(merged.secrets.is_empty());
    }
}
