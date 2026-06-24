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

use engram_egress_proxy::{CaSource, CertMint, Proxy, ProxyConfig, Registry};

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
    /// Load the CA via the supplied source, build the proxy, and
    /// spawn its listener.
    ///
    /// Best-effort: callers can recover from `EgressError::Bind` by
    /// running with egress disabled; the host-agent still serves
    /// sessions, but no egress filtering or broker-mode
    /// substitution. Operators see a warn-level log.
    pub async fn spawn(
        ca_source: Arc<dyn CaSource>,
        bind_addr: SocketAddr,
        observe_sink: Option<engram_egress_proxy::ObserveSink>,
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
        proxy_cfg.observe_sink = observe_sink;
        let proxy = Proxy::new(proxy_cfg);
        let task = tokio::spawn(async move {
            if let Err(e) = proxy.run().await {
                tracing::error!(error = %e, "egress proxy listener exited");
            }
        });
        tracing::info!(addr = %bind_addr, "host-agent egress proxy spawned");

        Ok(Self {
            registry,
            ca_cert_pem,
            _proxy_task: task,
        })
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
        injects.push(engram_egress_proxy::InjectEntry {
            secret: i.secret,
            header_name: i.header_name,
            header_template: i.header_template,
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: i.methods,
                path_globs: i.path_globs,
            },
        });
    }
    // ADR 0056 Phase 4: translate the policy's observe specs (no secret to
    // resolve — the asset map is pure) into the proxy's ObserveEntry. The proxy
    // emits an IntegrationAsset from a matching request's real response.
    let mut observes = Vec::with_capacity(policy.observes.len());
    for o in policy.observes {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&o.allow_hosts, &o.allow_host_patterns)?;
        observes.push(engram_egress_proxy::ObserveEntry {
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: o.methods,
                path_globs: o.path_globs,
            },
            provider: o.provider,
            asset_kind: o.asset_kind,
            surface: o.surface,
            success: match o.success_status_class.as_deref() {
                Some("2xx") => engram_egress_proxy::SuccessRule::StatusClass2xx,
                _ => engram_egress_proxy::SuccessRule::Always,
            },
            data: o.data,
            fetchable: o.fetchable,
        });
    }
    registry.register(engram_egress_proxy::SessionState {
        session_id: policy.session_id,
        guest_ip: policy.guest_ip,
        network_allow,
        secrets,
        injects,
        observes,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
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
                secrets: vec![],
                injects: vec![EgressInjectEntry {
                    secret: "dd-secret".into(),
                    header_name: "DD-API-KEY".into(),
                    header_template: "{}".into(),
                    allow_hosts: vec!["api.datadoghq.com".into()],
                    allow_host_patterns: vec![],
                    methods: vec!["GET".into()],
                    path_globs: vec!["/api/v2/logs*".into()],
                }],
                observes: vec![EgressObserveEntry {
                    allow_hosts: vec!["api.github.com".into()],
                    allow_host_patterns: vec![],
                    methods: vec!["POST".into()],
                    path_globs: vec!["/repos/*/issues".into()],
                    provider: "github".into(),
                    asset_kind: "issue".into(),
                    surface: "asset".into(),
                    success_status_class: Some("2xx".into()),
                    data: vec![("number".into(), "$.resp.number".into())],
                    fetchable: Some("$.resp.html_url".into()),
                }],
                secret_mode: SecretMode::Broker,
            },
        )
        .expect("register");

        let state = registry.lookup(guest_ip).expect("session registered");
        assert_eq!(state.injects.len(), 1);
        let inj = &state.injects[0];
        assert_eq!(inj.secret, "dd-secret");
        assert_eq!(inj.header_name, "DD-API-KEY");
        assert!(inj.allow.matches("api.datadoghq.com"));
        assert!(inj.policy.allows("GET", "/api/v2/logs/events"));
        assert!(!inj.policy.allows("POST", "/api/v2/logs/events"));

        // ADR 0056 Phase 4: the observe spec translates into a proxy ObserveEntry.
        assert_eq!(state.observes.len(), 1);
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
    }
}
