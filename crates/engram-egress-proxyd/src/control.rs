//! The daemon's control plane: the UDS server host-agent drives.
//!
//! One connection per client; frames are request/response in order.
//! `SyncPolicies` is the load-bearing op: sent on every connect, it
//! makes reconnection itself the recovery flow (roll, daemon respawn,
//! host-agent respawn — all the same code path, no watcher anywhere).
//!
//! `register_policy` (the wire→proxy translation) moved here from
//! `engram-host-agent/src/egress.rs` (ADR 0121); host-agent now ships
//! the wire `SessionEgressPolicy` over the socket and the translation
//! happens where the registry lives.

use std::sync::Arc;

use engram_egress_proto::{FromProxyd, HelloInfo, ToProxyd};
use engram_egress_proxy::{GuestGatewayRegistry, Listeners, Proxy, Registry};

pub(crate) struct ControlState {
    pub registry: Arc<Registry>,
    pub gateway: Arc<GuestGatewayRegistry>,
    pub hello: HelloInfo,
    /// Flipped by the `Shutdown` op; [`crate::run`] selects on it and
    /// returns 0. A signal, not `process::exit`, so the in-process
    /// test harness can run a daemon without arming a process kill.
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

/// Accept loop. Accept errors are logged and retried — the control
/// socket must stay alive for the daemon's whole life, or the next
/// host-agent generation will classify us wedged and kill us (which
/// is the correct failure mode, so no extra self-defense here).
pub(crate) async fn serve(listener: tokio::net::UnixListener, state: Arc<ControlState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_conn(stream, state).await {
                        // Disconnects are routine (probes, host-agent
                        // restarts); log at debug only.
                        tracing::debug!(error = %e, "control connection ended");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "control accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_conn(
    mut stream: tokio::net::UnixStream,
    state: Arc<ControlState>,
) -> std::io::Result<()> {
    loop {
        let msg: ToProxyd = engram_egress_proto::read_frame(&mut stream).await?;
        let reply = match msg {
            ToProxyd::Hello { proto_version } => {
                if proto_version != engram_egress_proto::PROTO_VERSION {
                    tracing::warn!(
                        theirs = proto_version,
                        ours = engram_egress_proto::PROTO_VERSION,
                        "control hello with foreign proto version"
                    );
                }
                // Answer with OUR HelloInfo regardless: the caller's
                // adopt decision needs the facts even (especially)
                // on a mismatch.
                FromProxyd::HelloAck(state.hello.clone())
            }
            ToProxyd::SyncPolicies(policies) => sync_policies(&state, policies),
            ToProxyd::ApplyPolicy(policy) => match register_policy(&state.registry, *policy) {
                Ok(()) => FromProxyd::Ok,
                Err(e) => FromProxyd::Err(format!("translate egress policy: {e}")),
            },
            ToProxyd::RemoveSession(session_id) => {
                unregister_session(&state, session_id);
                FromProxyd::Ok
            }
            ToProxyd::Health => FromProxyd::HealthReport {
                sessions: state.registry.live_count(),
            },
            ToProxyd::LookupGuest(guest_ip) => {
                FromProxyd::Guest(state.registry.lookup(guest_ip).map(|s| {
                    engram_egress_proto::GuestSummary {
                        session_id: s.session_id,
                        sandbox_id: s.sandbox_id,
                    }
                }))
            }
            ToProxyd::Decide { guest_ip, host } => {
                FromProxyd::Decision(state.registry.lookup(guest_ip).map(|s| {
                    match s.decide(&host) {
                        engram_egress_proxy::Decision::Reject => "reject",
                        engram_egress_proxy::Decision::Bypass => "bypass",
                        engram_egress_proxy::Decision::Intercept { .. } => "intercept",
                        engram_egress_proxy::Decision::OwnApp { .. } => "own-app",
                    }
                    .to_string()
                }))
            }
            ToProxyd::Shutdown => {
                tracing::info!("shutdown requested over control socket; exiting");
                let _ = engram_egress_proto::write_frame(&mut stream, &FromProxyd::Ok).await;
                let _ = state.shutdown.send(true);
                return Ok(());
            }
        };
        engram_egress_proto::write_frame(&mut stream, &reply).await?;
    }
}

/// The single session-teardown entry point (the `HostEgress::
/// unregister_session` contract): drop the registration AND fan
/// `session_closed` to every tunnel upstream, or pooled state leaks.
fn unregister_session(state: &ControlState, session_id: engram_core::SessionId) {
    state.registry.unregister(session_id);
    state.gateway.session_closed(session_id);
}

/// Full replace: register everything in the set, then drop any
/// registered session NOT in the set (the stale-entry prune).
///
/// A translate failure follows the ADR 0111 lossy posture — a config
/// defect can reduce a session's policy but must never STRAND it. So
/// a failed policy's session still lands in the keep-set: its
/// existing registration (if any) stays live under the last policy
/// that translated, instead of being pruned into a total egress
/// lockout. The per-apply path (`ApplyPolicy`) keeps erroring loudly
/// — only the bulk replace shields survivors.
fn sync_policies(
    state: &ControlState,
    policies: Vec<engram_core::types::egress::SessionEgressPolicy>,
) -> FromProxyd {
    let mut keep = std::collections::HashSet::with_capacity(policies.len());
    let mut failed = 0usize;
    let mut applied = 0usize;
    let total = policies.len();
    for policy in policies {
        let session_id = policy.session_id;
        // Shield the session from the prune below regardless of the
        // translate outcome — a defective UPDATE must not evict a
        // working registration.
        keep.insert(session_id);
        match register_policy(&state.registry, policy) {
            Ok(()) => applied += 1,
            Err(e) => {
                failed += 1;
                tracing::error!(
                    %session_id,
                    error = %e,
                    "sync: policy translate failed; any existing registration for the \
                     session stays live under its previous policy",
                );
            }
        }
    }
    let mut pruned = 0usize;
    for stale in state
        .registry
        .session_ids()
        .into_iter()
        .filter(|sid| !keep.contains(sid))
    {
        unregister_session(state, stale);
        pruned += 1;
    }
    tracing::info!(applied, failed, pruned, total, "policies synced");
    FromProxyd::Ok
}

/// Bind the proxy listeners, retrying briefly to ride a transient port
/// race — a daemon replacement racing its predecessor's socket
/// teardown, exactly the ADR 0083 shape this helper was born for in
/// host-agent. Still-failing after the budget = fatal (the caller
/// exits non-zero, the spawning host-agent fails closed).
pub(crate) async fn bind_with_retry(proxy: &Proxy) -> Result<Listeners, std::io::Error> {
    const ATTEMPTS: u32 = 5;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match proxy.bind().await {
            Ok(bound) => return Ok(bound),
            Err(e) => {
                tracing::warn!(
                    attempt,
                    max_attempts = ATTEMPTS,
                    error = %e,
                    "egress listeners bind failed; retrying",
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

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
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
pub(crate) fn register_policy(
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
            mint_source: i.mint_source,
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
        sandbox_id: policy.sandbox_id,
        guest_ip: policy.guest_ip,
        network_allow,
        allow_all,
        secrets,
        injects,
        observes,
        guest_services: policy.guest_services,
        tunnels: policy.tunnels,
        // ADR 0118: the session's own app hostnames, for the SNI short circuit.
        apps: policy.apps,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::image::SecretMode;
    use engram_core::{SandboxId, SessionId};
    use std::net::Ipv4Addr;

    fn policy(session_id: SessionId, ip: Ipv4Addr) -> SessionEgressPolicy {
        SessionEgressPolicy {
            session_id,
            sandbox_id: SandboxId::new(),
            guest_ip: ip,
            network_allow_hosts: vec!["example.com".into()],
            network_allow_host_patterns: vec![],
            allow_all: false,
            secrets: vec![],
            injects: vec![],
            observes: vec![],
            guest_services: vec![],
            tunnels: vec![],
            secret_mode: SecretMode::Literal,
            apps: vec![],
        }
    }

    fn state() -> ControlState {
        ControlState {
            registry: Arc::new(Registry::new()),
            gateway: Arc::new(GuestGatewayRegistry::default()),
            shutdown: tokio::sync::watch::channel(false).0,
            hello: HelloInfo {
                proto_version: engram_egress_proto::PROTO_VERSION,
                source_fingerprint: None,
                proxy_port: 8443,
                dns_port: 5353,
                gateway_port: 13338,
                ca_fingerprint: "test".into(),
                coord_url: "http://coord".into(),
            },
        }
    }

    #[test]
    fn sync_registers_and_prunes() {
        let state = state();
        let kept = SessionId::new();
        let stale = SessionId::new();
        // A pre-existing registration the sync set does not carry.
        register_policy(&state.registry, policy(stale, Ipv4Addr::new(10, 200, 0, 2))).unwrap();
        assert_eq!(state.registry.live_count(), 1);

        let reply = sync_policies(&state, vec![policy(kept, Ipv4Addr::new(10, 200, 0, 6))]);
        assert_eq!(reply, FromProxyd::Ok);
        assert_eq!(state.registry.live_count(), 1);
        assert!(state
            .registry
            .lookup(Ipv4Addr::new(10, 200, 0, 6))
            .is_some());
        assert!(state
            .registry
            .lookup(Ipv4Addr::new(10, 200, 0, 2))
            .is_none());
    }

    #[test]
    fn sync_skips_untranslatable_policies_and_keeps_the_rest() {
        let state = state();
        let good = SessionId::new();
        let mut bad = policy(SessionId::new(), Ipv4Addr::new(10, 200, 0, 10));
        // An invalid glob pattern fails HostList::from_manifest.
        bad.network_allow_host_patterns = vec!["[".into()];
        let reply = sync_policies(
            &state,
            vec![bad, policy(good, Ipv4Addr::new(10, 200, 0, 14))],
        );
        assert_eq!(reply, FromProxyd::Ok);
        assert_eq!(state.registry.live_count(), 1);
        assert!(state
            .registry
            .lookup(Ipv4Addr::new(10, 200, 0, 14))
            .is_some());
    }

    /// Engrams-review HIGH on #1408: a live session whose UPDATED
    /// policy carries one untranslatable entry must NOT be pruned into
    /// a total egress lockout by the sync's replace half. The failed
    /// update leaves the existing registration live under its previous
    /// policy (the ADR 0111 "reduce, never strand" posture).
    #[test]
    fn sync_keeps_a_live_session_whose_updated_policy_is_untranslatable() {
        let state = state();
        let session = SessionId::new();
        let ip = Ipv4Addr::new(10, 200, 0, 18);
        register_policy(&state.registry, policy(session, ip)).expect("initial apply");
        assert!(state.registry.lookup(ip).is_some());

        let mut update = policy(session, ip);
        update.network_allow_host_patterns = vec!["[".into()]; // untranslatable
        let reply = sync_policies(&state, vec![update]);
        assert_eq!(reply, FromProxyd::Ok);
        let survivor = state
            .registry
            .lookup(ip)
            .expect("the defective update must not strand the live session");
        // Still the PREVIOUS policy's behavior, not a lockout.
        assert!(matches!(
            survivor.decide("example.com"),
            engram_egress_proxy::Decision::Bypass
        ));
    }

    /// A capture-shaped policy (ADR 0080: assembled coordinator-side —
    /// see `session_boot::assemble_capture_egress_policy`, where the
    /// posture-mapping tests live) with a scoped allowlist must
    /// translate + register cleanly. Moved from `pooled_backend.rs`
    /// with the translation itself (ADR 0121).
    #[test]
    fn capture_shaped_allowlist_policy_registers() {
        let mut p = policy(SessionId::new(), Ipv4Addr::new(169, 254, 0, 2));
        p.network_allow_hosts = vec!["accounts.google.com".into()];
        p.network_allow_host_patterns = vec!["*.auth0.com".into()];
        let registry = Registry::new();
        register_policy(&registry, p).expect("proxy must accept the capture-egress allowlist");
    }

    /// A capture-shaped allow-all policy must register, and the proxy
    /// must bypass an arbitrary host under it (the dev posture for an
    /// image whose warm boot needs unrestricted network).
    #[test]
    fn capture_shaped_allow_all_policy_bypasses() {
        let mut p = policy(SessionId::new(), Ipv4Addr::new(169, 254, 0, 3));
        p.network_allow_hosts = Vec::new();
        p.allow_all = true;
        let guest_ip = p.guest_ip;
        let registry = Registry::new();
        register_policy(&registry, p).expect("register allow-all");
        let state = registry.lookup(guest_ip).expect("registered");
        assert!(matches!(
            state.decide("anything.example.com"),
            engram_egress_proxy::Decision::Bypass
        ));
    }

    #[test]
    fn sha256_hex_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
