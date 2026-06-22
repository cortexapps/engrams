//! Per-session state lookup.
//!
//! The proxy receives a connection on the host and recovers the
//! source IP via `peer_addr`. That IP uniquely identifies a sandbox
//! (each VM owns a `/30` and the guest is always `.2`). The
//! registry maps `guest_ip → SessionState` for fast accept-time
//! lookup.
//!
//! `register` / `unregister` are called by the coordinator at session
//! create / destroy. We keep both maps (`by_guest_ip`,
//! `by_session`) so unregister-by-session-id is O(1).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use engram_core::SessionId;
use parking_lot::RwLock;

use crate::policy::HostList;

/// Everything the proxy needs to make a decision for a connection
/// arriving from a given guest IP.
#[derive(Clone, Debug)]
pub struct SessionState {
    pub session_id: SessionId,
    pub guest_ip: Ipv4Addr,
    /// `manifest.network.allow_hosts` ∪ `allow_host_patterns`.
    /// Hosts on this list are reachable; the proxy splices traffic
    /// through if no per-secret rule applies.
    pub network_allow: HostList,
    /// Per-secret entries. Any of these whose `allow` matches the
    /// SNI triggers MITM + substitution.
    pub secrets: Vec<SecretEntry>,
    /// ADR 0056 (Plane B): per-credential injections. Any whose `allow`
    /// matches the SNI triggers MITM; the proxy then enforces the entry's
    /// `RequestPolicy` (method + path) and, on a match, adds its auth
    /// header. A request to an inject-gated host whose shape matches no
    /// entry is rejected. The guest never holds the injected secret.
    pub injects: Vec<InjectEntry>,
}

#[derive(Clone, Debug)]
pub struct SecretEntry {
    /// `engram_ph_<sess>_<hash>`. Embedded by `apply_secrets_to_env`
    /// in the guest's environment; substituted on outbound traffic
    /// when the destination matches.
    pub placeholder: String,
    /// The real secret. Lives only in this struct on the host.
    pub real_value: String,
    /// Hosts the real value may be substituted on. SNI matched against
    /// this list at MITM time.
    pub allow: HostList,
}

/// ADR 0056 (Plane B): a credential the proxy injects host-side. The
/// guest holds no placeholder; on an outbound request matching `allow`
/// (SNI) AND `policy` (method + path), the proxy adds
/// `header_name: <header_template with "{}" → secret>`. The secret lives
/// only in this struct, on the host.
#[derive(Clone, Debug)]
pub struct InjectEntry {
    /// The real credential — host-side only.
    pub secret: String,
    pub header_name: String,
    /// `{}` is replaced by `secret` (e.g. `"Bearer {}"`, or `"{}"`).
    pub header_template: String,
    /// Hosts (SNI) this injection applies to.
    pub allow: HostList,
    /// Request shapes this injection gates + applies to.
    pub policy: RequestPolicy,
}

/// ADR 0056: the request shapes a Plane-B injection gates + applies to.
/// Layered on top of the SNI host match. `methods` empty = any method;
/// `path_prefixes` empty = any path.
#[derive(Clone, Debug, Default)]
pub struct RequestPolicy {
    pub methods: Vec<String>,
    pub path_prefixes: Vec<String>,
}

impl RequestPolicy {
    /// Is `(method, path)` permitted? Method match is case-insensitive;
    /// path match is prefix. An empty list means "any".
    pub fn allows(&self, method: &str, path: &str) -> bool {
        (self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
            && (self.path_prefixes.is_empty()
                || self
                    .path_prefixes
                    .iter()
                    .any(|p| path.starts_with(p.as_str())))
    }
}

#[derive(Default)]
pub struct Registry {
    inner: RwLock<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    by_guest_ip: HashMap<Ipv4Addr, Arc<SessionState>>,
    by_session: HashMap<SessionId, Ipv4Addr>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces any prior registration for this `session_id`.
    pub fn register(&self, state: SessionState) {
        let guest_ip = state.guest_ip;
        let session_id = state.session_id;
        let arc = Arc::new(state);
        let mut g = self.inner.write();
        // Remove any stale guest_ip mapping if the session moved
        // (shouldn't happen — sandbox ids are stable — but defensive).
        if let Some(old_ip) = g.by_session.insert(session_id, guest_ip) {
            if old_ip != guest_ip {
                g.by_guest_ip.remove(&old_ip);
            }
        }
        g.by_guest_ip.insert(guest_ip, arc);
    }

    pub fn unregister(&self, session_id: SessionId) {
        let mut g = self.inner.write();
        if let Some(ip) = g.by_session.remove(&session_id) {
            g.by_guest_ip.remove(&ip);
        }
    }

    pub fn lookup(&self, guest_ip: Ipv4Addr) -> Option<Arc<SessionState>> {
        self.inner.read().by_guest_ip.get(&guest_ip).cloned()
    }

    pub fn live_count(&self) -> usize {
        self.inner.read().by_session.len()
    }
}

impl SessionState {
    /// Decision for a given destination host: bypass, reject, or MITM —
    /// and, when MITM, which secrets to substitute + which injections
    /// apply. A host with any matching secret OR injection is MITM'd;
    /// otherwise the `network_allow` list decides bypass vs reject.
    pub fn decide(&self, hostname: &str) -> Decision<'_> {
        let secrets: Vec<&SecretEntry> = self
            .secrets
            .iter()
            .filter(|s| s.allow.matches(hostname))
            .collect();
        let injects: Vec<&InjectEntry> = self
            .injects
            .iter()
            .filter(|i| i.allow.matches(hostname))
            .collect();
        if !secrets.is_empty() || !injects.is_empty() {
            return Decision::Intercept { secrets, injects };
        }
        if self.network_allow.matches(hostname) {
            Decision::Bypass
        } else {
            Decision::Reject
        }
    }

    /// All placeholders in this session, regardless of which secret
    /// they belong to. Used by the violation scanner: a placeholder
    /// destined for a non-allowed host is a footgun even if the
    /// connection itself is otherwise allowed.
    pub fn all_placeholders(&self) -> Vec<&str> {
        self.secrets
            .iter()
            .map(|s| s.placeholder.as_str())
            .collect()
    }
}

#[derive(Debug)]
pub enum Decision<'a> {
    /// SNI not in `network_allow` and no secret/injection applies. Drop.
    Reject,
    /// SNI in `network_allow`, nothing to substitute or inject. Splice through.
    Bypass,
    /// One or more secrets/injections apply. MITM, then substitute
    /// placeholders (`secrets`) and/or inject + gate (`injects`, ADR 0056).
    Intercept {
        secrets: Vec<&'a SecretEntry>,
        injects: Vec<&'a InjectEntry>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn state() -> SessionState {
        SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::from_str("10.200.0.2").unwrap(),
            network_allow: HostList::from_manifest(
                &["api.github.com".into(), "registry.npmjs.org".into()],
                &[],
            )
            .unwrap(),
            secrets: vec![SecretEntry {
                placeholder: "engram_ph_xxx_yyy".into(),
                real_value: "sk-real".into(),
                allow: HostList::from_manifest(&["api.openai.com".into()], &[]).unwrap(),
            }],
            injects: vec![InjectEntry {
                secret: "dd-secret".into(),
                header_name: "DD-API-KEY".into(),
                header_template: "{}".into(),
                allow: HostList::from_manifest(&["api.datadoghq.com".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["GET".into()],
                    path_prefixes: vec!["/api/v2/logs".into()],
                },
            }],
        }
    }

    #[test]
    fn decision_intercept_when_secret_allows() {
        assert!(matches!(
            state().decide("api.openai.com"),
            Decision::Intercept { .. }
        ));
    }

    #[test]
    fn decision_intercept_when_inject_allows() {
        // ADR 0056: an inject host MITMs even though it's not a secret host
        // and not in network_allow — the request-policy gating happens at
        // intercept time, not here.
        match state().decide("api.datadoghq.com") {
            Decision::Intercept { secrets, injects } => {
                assert!(secrets.is_empty());
                assert_eq!(injects.len(), 1);
                assert_eq!(injects[0].header_name, "DD-API-KEY");
            }
            other => panic!("expected Intercept, got {other:?}"),
        }
    }

    #[test]
    fn decision_bypass_when_only_network_allows() {
        assert!(matches!(state().decide("api.github.com"), Decision::Bypass));
    }

    #[test]
    fn decision_reject_when_neither_matches() {
        assert!(matches!(state().decide("api.evil.com"), Decision::Reject));
    }

    #[test]
    fn registry_lookup_round_trip() {
        let r = Registry::new();
        let s = state();
        let ip = s.guest_ip;
        let sid = s.session_id;
        r.register(s);
        assert_eq!(r.live_count(), 1);
        assert!(r.lookup(ip).is_some());
        r.unregister(sid);
        assert_eq!(r.live_count(), 0);
        assert!(r.lookup(ip).is_none());
    }

    #[test]
    fn unregister_unknown_session_is_noop() {
        let r = Registry::new();
        r.unregister(SessionId::new());
    }

    #[test]
    fn placeholders_returns_each_secret() {
        let mut s = state();
        s.secrets.push(SecretEntry {
            placeholder: "engram_ph_aaa_bbb".into(),
            real_value: "sk-other".into(),
            allow: HostList::from_manifest(&["api.anthropic.com".into()], &[]).unwrap(),
        });
        let p = s.all_placeholders();
        assert_eq!(p.len(), 2);
        assert!(p.contains(&"engram_ph_xxx_yyy"));
        assert!(p.contains(&"engram_ph_aaa_bbb"));
    }
}
