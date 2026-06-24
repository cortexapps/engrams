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
    /// ADR 0056 (Phase 4): response-observation specs. Any whose `allow`
    /// matches the SNI triggers MITM; for a request whose shape matches the
    /// entry's `RequestPolicy`, the proxy buffers + parses the *response* and
    /// emits an `IntegrationAsset` (the side-effect ⟹ event invariant). An
    /// observe-only host (no secret, no inject) is MITM'd purely to observe.
    pub observes: Vec<ObserveEntry>,
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
///
/// `path_prefixes` are **glob patterns** — a `*` matches any run of characters
/// (including `/`) — matched against the whole request path. The name is
/// historical: the orchestrator used to truncate the connector match path at the
/// first `*` and prefix-match the literal head, so `/repos/*/pulls` collapsed to
/// `/repos/` and the gate + observe fired on *any* `/repos/…` request (a coarse
/// over-match — e.g. a branch-creation `POST /repos/o/r/git/refs` got the token
/// injected and emitted a junk `pull_request` asset). It now carries the whole
/// pattern and globs it, so `POST /repos/o/r/pulls` matches `/repos/*/pulls`
/// while `POST /repos/o/r/git/refs` does not.
#[derive(Clone, Debug, Default)]
pub struct RequestPolicy {
    pub methods: Vec<String>,
    pub path_prefixes: Vec<String>,
}

impl RequestPolicy {
    /// Is `(method, path)` permitted? Method match is case-insensitive; path
    /// match is a glob over the whole path (`*` = any chars). Empty list = "any".
    pub fn allows(&self, method: &str, path: &str) -> bool {
        (self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
            && (self.path_prefixes.is_empty()
                || self.path_prefixes.iter().any(|p| glob_match(p, path)))
    }
}

/// Glob match: `*` matches any run of characters (including `/`); the pattern
/// must match the ENTIRE `text`. Iterative with backtracking (O(n·m), no
/// recursion blow-up). `*` is the only metacharacter — connector match paths use
/// nothing else.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Backtrack point: the last `*` seen, and where in `text` we resumed it.
    let (mut star, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            // Mismatch under a `*`: let the `*` swallow one more char of `text`.
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern match the empty remainder.
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// ADR 0056 (Phase 4): when does an observed response count as a successful
/// side effect worth recording? The proxy evaluates this against the parsed
/// response status. Omitted in the connector → `Always`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SuccessRule {
    /// Record only when the response status is 2xx.
    #[default]
    StatusClass2xx,
    /// Record on any status (the connector omitted a success rule).
    Always,
}

impl SuccessRule {
    /// Does `status` satisfy this rule? `None` (unparseable status) is treated
    /// as "indeterminate" — see [`crate::observe`] for the coarse-emit posture.
    pub fn satisfied_by(&self, status: u16) -> bool {
        match self {
            Self::StatusClass2xx => (200..300).contains(&status),
            Self::Always => true,
        }
    }
}

/// ADR 0056 (Phase 4): one response-observation spec. On an outbound request
/// matching `allow` (SNI) AND `policy` (method + path), the proxy buffers the
/// response and — when `success` is satisfied — emits an `IntegrationAsset`
/// built from `data`/`fetchable` (uniform `$.resp.*` / `$.req.*` / `$.status`
/// extractor paths). Opaque `surface` ("action" | "asset") — the coordinator
/// maps it to its `AssetSurface`. The proxy never asserts asset truth from the
/// guest; it reads the real response bytes.
#[derive(Clone, Debug)]
pub struct ObserveEntry {
    pub allow: HostList,
    pub policy: RequestPolicy,
    pub provider: String,
    pub asset_kind: String,
    pub surface: String,
    pub success: SuccessRule,
    /// `(field name, extractor path)` pairs, e.g. `("number", "$.resp.number")`.
    pub data: Vec<(String, String)>,
    /// Extractor path yielding an external URL, e.g. `"$.resp.html_url"`.
    pub fetchable: Option<String>,
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
        let observes: Vec<&ObserveEntry> = self
            .observes
            .iter()
            .filter(|o| o.allow.matches(hostname))
            .collect();
        if !secrets.is_empty() || !injects.is_empty() || !observes.is_empty() {
            return Decision::Intercept {
                secrets,
                injects,
                observes,
            };
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
    /// One or more secrets/injections/observations apply. MITM, then
    /// substitute placeholders (`secrets`), inject + gate (`injects`), and/or
    /// observe the response (`observes`, ADR 0056 Phase 4).
    Intercept {
        secrets: Vec<&'a SecretEntry>,
        injects: Vec<&'a InjectEntry>,
        observes: Vec<&'a ObserveEntry>,
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
                    path_prefixes: vec!["/api/v2/logs*".into()],
                },
            }],
            observes: vec![ObserveEntry {
                allow: HostList::from_manifest(&["api.github.com".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["POST".into()],
                    path_prefixes: vec!["/repos/*/issues".into()],
                },
                provider: "github".into(),
                asset_kind: "issue".into(),
                surface: "asset".into(),
                success: SuccessRule::StatusClass2xx,
                data: vec![("number".into(), "$.resp.number".into())],
                fetchable: Some("$.resp.html_url".into()),
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
            Decision::Intercept {
                secrets,
                injects,
                observes,
            } => {
                assert!(secrets.is_empty());
                assert_eq!(injects.len(), 1);
                assert_eq!(injects[0].header_name, "DD-API-KEY");
                assert!(observes.is_empty());
            }
            other => panic!("expected Intercept, got {other:?}"),
        }
    }

    #[test]
    fn decision_intercept_when_observe_allows() {
        // ADR 0056 Phase 4: an observe-only host MITMs (to read the response)
        // even though it carries no secret/inject — it IS in network_allow but
        // the observe spec forces Intercept over Bypass.
        match state().decide("api.github.com") {
            Decision::Intercept {
                secrets,
                injects,
                observes,
            } => {
                assert!(secrets.is_empty());
                assert!(injects.is_empty());
                assert_eq!(observes.len(), 1);
                assert_eq!(observes[0].asset_kind, "issue");
            }
            other => panic!("expected Intercept, got {other:?}"),
        }
    }

    #[test]
    fn decision_bypass_when_only_network_allows() {
        // registry.npmjs.org is in network_allow with no secret/inject/observe.
        assert!(matches!(
            state().decide("registry.npmjs.org"),
            Decision::Bypass
        ));
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

    #[test]
    fn glob_match_semantics() {
        // No metacharacter → exact, whole-string match (the granularity fix:
        // `/repos/*/pulls` must NOT leak into `/repos/o/r/git/refs`).
        assert!(glob_match("/repos/o/r/pulls", "/repos/o/r/pulls"));
        assert!(!glob_match("/repos/o/r/pulls", "/repos/o/r/pulls/1"));
        assert!(!glob_match("/repos/o/r/pulls", "/repos/o/r"));

        // `*` swallows any run of chars, including `/`.
        assert!(glob_match("/repos/*/pulls", "/repos/octo/repo/pulls"));
        assert!(glob_match("/repos/*/pulls", "/repos/a/b/c/pulls")); // owner segment can contain slashes
        assert!(!glob_match("/repos/*/pulls", "/repos/octo/repo/git/refs"));

        // Trailing `*` matches the empty remainder AND query strings / sub-paths.
        assert!(glob_match("/api/v2/logs*", "/api/v2/logs"));
        assert!(glob_match("/api/v2/logs*", "/api/v2/logs/events?query=x"));
        assert!(glob_match(
            "/repos/*/issues*",
            "/repos/o/r/issues?state=open"
        ));
        assert!(glob_match("/repos/*/issues*", "/repos/o/r/issues/42")); // issue comments etc.

        // Multiple `*` and leading `*`.
        assert!(glob_match(
            "/repos/*/contents/*",
            "/repos/o/r/contents/path/to/file"
        ));
        assert!(!glob_match("/repos/*/contents/*", "/repos/o/r/contents")); // needs the trailing segment
        assert!(glob_match("*", "/anything/at/all"));
        assert!(glob_match("*", ""));

        // A bare `*` mid-pattern that must backtrack to a later literal.
        assert!(glob_match("/a/*/b", "/a/x/y/b"));
        assert!(!glob_match("/a/*/b", "/a/x/y/c"));
    }
}
