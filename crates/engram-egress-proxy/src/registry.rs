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
    /// Decision for a given destination host: which (if any) secrets
    /// should the proxy try to substitute?
    pub fn decide(&self, hostname: &str) -> Decision<'_> {
        let mut applicable: Vec<&SecretEntry> = Vec::new();
        for secret in &self.secrets {
            if secret.allow.matches(hostname) {
                applicable.push(secret);
            }
        }
        let in_network_allow = self.network_allow.matches(hostname);
        match (applicable.is_empty(), in_network_allow) {
            (true, true) => Decision::Bypass,
            (true, false) => Decision::Reject,
            (false, _) => Decision::Intercept(applicable),
        }
    }

    /// All placeholders in this session, regardless of which secret
    /// they belong to. Used by the violation scanner: a placeholder
    /// destined for a non-allowed host is a footgun even if the
    /// connection itself is otherwise allowed.
    pub fn all_placeholders(&self) -> Vec<&str> {
        self.secrets.iter().map(|s| s.placeholder.as_str()).collect()
    }
}

#[derive(Debug)]
pub enum Decision<'a> {
    /// SNI not in `network_allow` and no secret allows it. Drop.
    Reject,
    /// SNI in `network_allow` but no secret applies. Splice through.
    Bypass,
    /// One or more secrets allow this destination. MITM + substitute.
    Intercept(Vec<&'a SecretEntry>),
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
        }
    }

    #[test]
    fn decision_intercept_when_secret_allows() {
        assert!(matches!(state().decide("api.openai.com"), Decision::Intercept(_)));
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
