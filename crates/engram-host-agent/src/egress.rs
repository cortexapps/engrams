//! Host-agent-owned egress proxy.
//!
//! ADR 0006: each FC host-agent runs its own TLS-MITM proxy.
//! iptables REDIRECT on the FC host hands tcp/443 from every guest
//! to this proxy. The proxy looks up the source IP in the local
//! registry (populated by `SandboxBackend::notify_session_policy`
//! frames from the coordinator), applies the manifest's network
//! allow-list at SNI, and substitutes per-secret placeholders for
//! broker-mode images after MITM.
//!
//! Every host-agent in a deployment loads the same CA material via
//! a [`CaSource`] impl so guest substrates that trust one host's
//! leaves trust them all.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::{HostId, SessionId};
use engram_egress_proxy::{
    CaSource, CertMint, InjectRefresher, Listeners, Proxy, ProxyConfig, RefreshedInject, Registry,
};

use crate::coord_client::HttpCoordClient;

/// WS4: the host-agent's [`InjectRefresher`] — bridges the egress proxy's
/// near-expiry re-mint request to the coordinator's inject-refresh route (which
/// holds the mint authority). The proxy AWAITS this before injecting a stale
/// minted credential, closing the campaign's reads-401/writes-succeed asymmetry.
pub struct CoordInjectRefresher {
    coord: HttpCoordClient,
    host_id: HostId,
}

impl CoordInjectRefresher {
    pub fn new(coord: HttpCoordClient, host_id: HostId) -> Self {
        Self { coord, host_id }
    }
}

#[async_trait]
impl InjectRefresher for CoordInjectRefresher {
    async fn refresh(&self, session_id: SessionId, mint_provider: &str) -> Option<RefreshedInject> {
        match self
            .coord
            .refresh_inject(self.host_id, session_id, mint_provider)
            .await
        {
            Ok(resp) => Some(RefreshedInject {
                secret: resp.secret,
                expires_at: resp.expires_at,
            }),
            Err(e) => {
                // The proxy keeps the stale secret on `None` — a stale token 401s
                // (recoverable), and a coord blip must not drop the guest's request.
                tracing::warn!(%session_id, provider = %mint_provider, error = %e, "egress inject re-mint via coord failed");
                None
            }
        }
    }
}

/// Per-host egress-proxy handle. Holds the registry (mutated as
/// sessions come and go on this host), the CA cert PEM (handed to
/// `ensure_harness_ext4` so every guest substrate this host builds
/// trusts our leaves), and the spawned listener task.
pub struct HostEgress {
    pub registry: Arc<Registry>,
    /// CA cert PEM. Stamped into every harness substrate this host
    /// builds so the guest's trust store accepts our MITM leaves.
    pub ca_cert_pem: String,
    /// Listener task. Held for its lifetime; dropping the
    /// `HostEgress` doesn't abort the task because the registry +
    /// mint are held inside the proxy via Arc, so the future
    /// stays valid. We keep the handle anyway so callers can
    /// observe spawn/error.
    _proxy_task: tokio::task::JoinHandle<()>,
}

/// Reasons proxy spawn-up can fail. Distinguishes CA loading
/// (typically a deployment misconfig) from binding (port in use).
#[derive(Debug)]
pub enum EgressError {
    Ca(engram_egress_proxy::CaError),
    /// CA loaded but the listener couldn't bind. Returned with the
    /// underlying io error so operators see the exact reason.
    Bind(std::io::Error),
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ca(e) => write!(f, "egress CA: {e}"),
            Self::Bind(e) => write!(f, "egress proxy bind: {e}"),
        }
    }
}

impl std::error::Error for EgressError {}

impl HostEgress {
    /// Load the CA via the supplied source, build the proxy, **bind its
    /// listeners synchronously**, then spawn the accept loop.
    ///
    /// Fail-closed on a bind failure. Egress is mandatory (issue #240):
    /// the caller (`main.rs`) aborts host-agent startup on
    /// `EgressError::Bind`, because a host that keeps running with a
    /// live iptables `:443 -> proxy` REDIRECT but no listener sends
    /// every guest a RST — the guest's TLS client reports
    /// `ConnectionRefused` — silently breaking every session on the
    /// host. Binding here (rather than inside the accept-loop task)
    /// is what turns that failure into a value the caller can act on;
    /// the earlier design spawned the bind inside a detached task, so
    /// `spawn` returned `Ok` before the bind was even attempted and a
    /// failure only surfaced as a log line from the dying task.
    /// `dns_bind_addr`: where the filtering DNS proxy binds (both UDP
    /// and TCP). Production passes the port the iptables `:53 -> dns`
    /// REDIRECT targets; it must match or the guest can't resolve.
    /// `None` disables the DNS listener entirely — used by tests that
    /// exercise only the egress registry and would otherwise collide on
    /// the fixed DNS port when the suite runs in parallel.
    pub async fn spawn(
        ca_source: Arc<dyn CaSource>,
        bind_addr: SocketAddr,
        dns_bind_addr: Option<SocketAddr>,
        observe_sink: Option<engram_egress_proxy::ObserveSink>,
        inject_refresher: Option<Arc<dyn InjectRefresher>>,
    ) -> Result<Self, EgressError> {
        let ca = ca_source.load().await.map_err(EgressError::Ca)?;
        let ca_cert_pem = ca.cert_pem.clone();

        // Install rustls's default crypto provider once. The proxy
        // signs leaves via a single global provider; `install_default`
        // returns Err if already set (typical when --mode=all runs
        // both coord and host-agent in-process and another caller
        // set the provider first). Ignore that.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let registry = Arc::new(Registry::new());
        let mint = Arc::new(CertMint::new(Arc::new(ca)));

        let mut proxy_cfg = ProxyConfig::new(bind_addr, registry.clone(), mint);
        proxy_cfg.dns_bind_addr = dns_bind_addr;
        proxy_cfg.observe_sink = observe_sink;
        proxy_cfg.inject_refresher = inject_refresher;
        let proxy = Proxy::new(proxy_cfg);

        let listeners = bind_with_retry(&proxy).await.map_err(EgressError::Bind)?;
        let task = tokio::spawn(async move {
            proxy.serve(listeners).await;
            // `serve` loops forever on accept; if it ever returns, the
            // proxy is down while iptables still REDIRECTs to it —
            // log loudly so operators aren't left diagnosing silent
            // per-session `ConnectionRefused`.
            tracing::error!("egress proxy serve loop exited unexpectedly");
        });
        tracing::info!(addr = %bind_addr, "host-agent egress proxy spawned");

        Ok(Self {
            registry,
            ca_cert_pem,
            _proxy_task: task,
        })
    }
}

/// Bind the proxy listeners, retrying briefly to ride over a transient
/// port race — e.g. a host-agent restart racing the previous instance's
/// socket teardown, which is exactly how the proxy came up dead on the
/// fc-colima dev rig (both ports were free moments later). A bind that
/// still fails after the retry budget is fatal: `spawn` returns
/// `EgressError::Bind` and `main.rs` aborts (fail-closed). Under a
/// supervisor (K8s, or a Tilt retrigger) a permanent conflict then
/// crashloops loudly instead of serving broken sessions.
async fn bind_with_retry(proxy: &Proxy) -> Result<Listeners, std::io::Error> {
    const ATTEMPTS: u32 = 5;
    const BACKOFF: Duration = Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match proxy.bind().await {
            Ok(bound) => return Ok(bound),
            Err(e) => {
                tracing::warn!(
                    attempt,
                    max_attempts = ATTEMPTS,
                    error = %e,
                    "egress proxy bind failed; retrying",
                );
                last_err = Some(e);
                if attempt < ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once, so last_err is set on failure"))
}

/// ADR 0059: build the proxy's optional GraphQL matcher from the wire fields.
/// `Ok(None)` = a REST entry (empty op); `Ok(Some(_))` = a valid GraphQL entry;
/// `Err(())` = a non-empty but unparseable op (a corrupt entry — the caller skips
/// it, fail-closed, so it never degrades into a permissive REST gate on `/graphql`).
fn graphql_match(op: &str, field: &str) -> Result<Option<engram_egress_proxy::GraphqlMatch>, ()> {
    if op.is_empty() {
        return Ok(None);
    }
    match engram_egress_proxy::GraphqlOperation::parse(op) {
        Some(operation) => Ok(Some(engram_egress_proxy::GraphqlMatch {
            operation,
            field: field.to_string(),
        })),
        None => Err(()),
    }
}

/// Translate an incoming wire policy into the proxy's `SessionState`
/// shape and register it against `guest_ip`. The proxy looks up
/// sessions by IP on every connection — this is the source of
/// truth for "does the proxy know about this session yet?".
///
/// Idempotent on re-registration: `Registry::register` replaces
/// any prior entry for the same IP, so a re-issued policy frame
/// (e.g. on cold resume to a new host that previously held the
/// session) updates cleanly.
pub fn register_policy(
    registry: &Registry,
    policy: engram_core::types::egress::SessionEgressPolicy,
) -> Result<(), engram_egress_proxy::policy::ParseError> {
    let network_allow = engram_egress_proxy::HostList::from_manifest(
        &policy.network_allow_hosts,
        &policy.network_allow_host_patterns,
    )?;
    let allow_all = policy.allow_all;
    let mut secrets = Vec::with_capacity(policy.secrets.len());
    for s in policy.secrets {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&s.allow_hosts, &s.allow_host_patterns)?;
        secrets.push(engram_egress_proxy::SecretEntry {
            placeholder: s.placeholder,
            real_value: s.real_value,
            allow,
        });
    }
    // ADR 0056 Plane B: the coordinator already resolved each inject's
    // secret_ref → real `secret` (host-side); translate into the proxy's
    // InjectEntry + RequestPolicy.
    let mut injects = Vec::with_capacity(policy.injects.len());
    for i in policy.injects {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&i.allow_hosts, &i.allow_host_patterns)?;
        // ADR 0059: a non-empty-but-unparseable graphql op is a corrupt entry —
        // skip it (fail-closed) so it never degrades into a permissive REST gate
        // on `/graphql`.
        let graphql = match graphql_match(&i.graphql_operation, &i.graphql_field) {
            Ok(g) => g,
            Err(()) => {
                tracing::warn!(
                    op = %i.graphql_operation,
                    "skipping inject with unparseable graphql operation",
                );
                continue;
            }
        };
        injects.push(engram_egress_proxy::InjectEntry {
            header_name: i.header_name,
            header_template: i.header_template,
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: i.methods,
                path_globs: i.path_globs,
                graphql,
            },
            // WS4: a minted entry (non-empty provider) carries a TTL the proxy
            // re-mints against near expiry; a static secret has neither.
            mint_provider: i.mint_provider,
            cred: engram_egress_proxy::RefreshableCred::new(i.secret, i.expires_at),
        });
    }
    // ADR 0056 Phase 4: translate the policy's observe specs (no secret to
    // resolve — the asset map is pure) into the proxy's ObserveEntry. The proxy
    // emits an IntegrationAsset from a matching request's real response.
    let mut observes = Vec::with_capacity(policy.observes.len());
    for o in policy.observes {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&o.allow_hosts, &o.allow_host_patterns)?;
        let graphql = match graphql_match(&o.graphql_operation, &o.graphql_field) {
            Ok(g) => g,
            Err(()) => {
                tracing::warn!(
                    op = %o.graphql_operation,
                    "skipping observe with unparseable graphql operation",
                );
                continue;
            }
        };
        // ADR 0059: a GraphQL observe gates success on the absence of top-level
        // `errors`; a REST observe on the 2xx status class (else Always).
        let success = if o.success_no_graphql_errors {
            engram_egress_proxy::SuccessRule::NoGraphqlErrors
        } else {
            match o.success_status_class.as_deref() {
                Some("2xx") => engram_egress_proxy::SuccessRule::StatusClass2xx,
                _ => engram_egress_proxy::SuccessRule::Always,
            }
        };
        observes.push(engram_egress_proxy::ObserveEntry {
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: o.methods,
                path_globs: o.path_globs,
                graphql,
            },
            provider: o.provider,
            asset_kind: o.asset_kind,
            surface: o.surface,
            success,
            data: o.data,
            fetchable: o.fetchable,
            url_fallback: o.url_fallback.map(|f| engram_egress_proxy::UrlFallback {
                pattern: f.pattern,
                fields: f.fields,
            }),
        });
    }
    registry.register(engram_egress_proxy::SessionState {
        session_id: policy.session_id,
        guest_ip: policy.guest_ip,
        network_allow,
        allow_all,
        secrets,
        injects,
        observes,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use engram_core::types::egress::{EgressInjectEntry, EgressObserveEntry, SessionEgressPolicy};
    use engram_core::types::image::SecretMode;
    use engram_core::{SandboxId, SessionId};
    use std::net::Ipv4Addr;

    /// ADR 0056 (B′): `register_policy` translates a wire `EgressInjectEntry`
    /// (secret already resolved by the coordinator) into the proxy's
    /// `InjectEntry` + `RequestPolicy` the 3a engine enforces.
    #[test]
    fn register_policy_translates_injects_to_proxy_entries() {
        let registry = Registry::new();
        let guest_ip = Ipv4Addr::new(10, 200, 0, 2);
        register_policy(
            &registry,
            SessionEgressPolicy {
                session_id: SessionId::new(),
                sandbox_id: SandboxId::new(),
                guest_ip,
                network_allow_hosts: vec![],
                network_allow_host_patterns: vec![],
                allow_all: false,
                secrets: vec![],
                injects: vec![
                    EgressInjectEntry {
                        secret: "dd-secret".into(),
                        header_name: "DD-API-KEY".into(),
                        header_template: "{}".into(),
                        allow_hosts: vec!["api.datadoghq.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["GET".into()],
                        path_globs: vec!["/api/v2/logs*".into()],
                        graphql_operation: String::new(),
                        graphql_field: String::new(),
                        mint_provider: String::new(),
                        expires_at: None,
                    },
                    // ADR 0059: a GraphQL inject (gated by operation+field).
                    // WS4: a minted (refreshable) github entry with a TTL.
                    EgressInjectEntry {
                        secret: "gh-token".into(),
                        header_name: "Authorization".into(),
                        header_template: "Bearer {}".into(),
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["POST".into()],
                        path_globs: vec!["/graphql".into()],
                        graphql_operation: "mutation".into(),
                        graphql_field: "mergePullRequest".into(),
                        mint_provider: "github".into(),
                        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    },
                ],
                observes: vec![
                    EgressObserveEntry {
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
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
                        url_fallback: None,
                    },
                    // ADR 0059: a GraphQL observe (NoGraphqlErrors success rule).
                    EgressObserveEntry {
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
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
                        url_fallback: Some(engram_core::types::integration::ObserveUrlFallback {
                            pattern: "https://github.com/{owner}/{name}/issues/{number:int}".into(),
                            fields: vec![("number".into(), "{number}".into())],
                        }),
                    },
                ],
                secret_mode: SecretMode::Broker,
            },
        )
        .expect("register");

        let state = registry.lookup(guest_ip).expect("session registered");
        assert_eq!(state.injects.len(), 2);
        let inj = &state.injects[0];
        assert_eq!(inj.secret(), "dd-secret");
        assert_eq!(inj.mint_provider, ""); // static: never refreshed
        assert_eq!(inj.header_name, "DD-API-KEY");
        assert!(inj.allow.matches("api.datadoghq.com"));
        assert!(inj.policy.allows("GET", "/api/v2/logs/events"));
        assert!(!inj.policy.allows("POST", "/api/v2/logs/events"));
        assert!(
            inj.policy.graphql.is_none(),
            "REST inject has no graphql matcher"
        );

        // ADR 0059: the GraphQL inject translates into a RequestPolicy.graphql.
        let gql_inj = &state.injects[1];
        assert_eq!(gql_inj.secret(), "gh-token");
        assert_eq!(gql_inj.mint_provider, "github"); // WS4: refreshable
        let g = gql_inj
            .policy
            .graphql
            .as_ref()
            .expect("graphql matcher present");
        assert_eq!(g.operation, engram_egress_proxy::GraphqlOperation::Mutation);
        assert_eq!(g.field, "mergePullRequest");
        assert!(gql_inj.policy.path_matches("/graphql"));

        // ADR 0056 Phase 4: the REST observe spec translates into a proxy ObserveEntry.
        assert_eq!(state.observes.len(), 2);
        let obs = &state.observes[0];
        assert_eq!(obs.provider, "github");
        assert_eq!(obs.asset_kind, "issue");
        assert_eq!(obs.surface, "asset");
        assert!(obs.allow.matches("api.github.com"));
        assert!(obs.policy.allows("POST", "/repos/x/issues"));
        assert!(matches!(
            obs.success,
            engram_egress_proxy::SuccessRule::StatusClass2xx
        ));
        assert_eq!(obs.fetchable.as_deref(), Some("$.resp.html_url"));

        // ADR 0059: the GraphQL observe maps to a graphql matcher + NoGraphqlErrors.
        let gql_obs = &state.observes[1];
        let go = gql_obs
            .policy
            .graphql
            .as_ref()
            .expect("graphql matcher present");
        assert_eq!(
            go.operation,
            engram_egress_proxy::GraphqlOperation::Mutation
        );
        assert_eq!(go.field, "createIssue");
        assert!(matches!(
            gql_obs.success,
            engram_egress_proxy::SuccessRule::NoGraphqlErrors
        ));
        // GraphQL parity: the URL fallback rides through to the proxy entry.
        let fb = gql_obs.url_fallback.as_ref().expect("url fallback present");
        assert_eq!(
            fb.pattern,
            "https://github.com/{owner}/{name}/issues/{number:int}"
        );
        assert_eq!(fb.fields, vec![("number".into(), "{number}".into())]);
        assert_eq!(state.observes[0].url_fallback, None);
    }
}
