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

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
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
    /// Allow ALL egress, bypassing `network_allow` entirely (a `[network]
    /// default = "allow"` posture). The capture VM's `[warm]` hook uses this
    /// on dev images where no agent runs at capture. Per-secret/inject/observe
    /// MITM rules still take precedence (they're matched first in `decide`).
    pub allow_all: bool,
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
///
/// WS4: the credential is *refreshable*. A minted entry (`mint_provider`
/// non-empty) carries a short-lived credential (a GitHub App installation
/// token, ~1h TTL) that the proxy re-mints via its [`InjectRefresher`] seam
/// near expiry — closing the campaign's reads-401/writes-succeed asymmetry
/// where the boot-time inject token was minted ONCE and went stale ~1h later.
#[derive(Clone, Debug)]
pub struct InjectEntry {
    pub header_name: String,
    /// `{}` is replaced by the current secret (e.g. `"Bearer {}"`, or `"{}"`).
    pub header_template: String,
    /// Hosts (SNI) this injection applies to.
    pub allow: HostList,
    /// Request shapes this injection gates + applies to.
    pub policy: RequestPolicy,
    /// WS4: the mint provider whose credential this injects (e.g. `"github"`),
    /// or empty for a static/non-refreshable secret. Non-empty ⇒ the proxy
    /// re-mints `cred` via the [`InjectRefresher`] near expiry.
    pub mint_provider: String,
    /// WS4: the refreshable credential cell. [`Self::secret`] reads the current
    /// value; [`Self::refresh_if_stale`] re-mints it (single-flighted) when a
    /// near-expiry request arrives. The secret lives only here, on the host.
    pub cred: Arc<RefreshableCred>,
}

impl InjectEntry {
    /// The current secret to inject (host-side only).
    pub fn secret(&self) -> String {
        self.cred.secret()
    }

    /// WS4: if this is a refreshable (minted) entry within 5 min of expiry,
    /// re-mint it via `refresher`, single-flighted so concurrent connections
    /// re-mint at most once. On refresh failure the STALE secret is kept — a
    /// request under a stale token 401s (recoverable), whereas dropping the
    /// request is not. A no-op for a static entry (empty `mint_provider`) or one
    /// still comfortably inside its validity window.
    pub async fn refresh_if_stale(&self, session_id: SessionId, refresher: &dyn InjectRefresher) {
        if self.mint_provider.is_empty() || self.cred.fresh_enough() {
            return;
        }
        // Single-flight: hold the async guard across the re-mint. Late arrivals
        // block here, then re-check and observe the freshly-minted value.
        let _guard = self.cred.refreshing.lock().await;
        if self.cred.fresh_enough() {
            return; // another connection refreshed while we waited
        }
        match refresher.refresh(session_id, &self.mint_provider).await {
            Some(fresh) => {
                *self.cred.current.write() = CredState {
                    secret: fresh.secret,
                    expires_at: Some(fresh.expires_at),
                };
            }
            None => tracing::warn!(
                provider = %self.mint_provider,
                %session_id,
                "egress inject refresh failed; keeping the stale credential \
                 (a 401 is recoverable; a dropped request is not)",
            ),
        }
    }
}

/// WS4: the mutable, single-flighted credential behind an [`InjectEntry`]. Reads
/// (`secret`) take a short `parking_lot` read lock; a refresh holds the async
/// `refreshing` mutex so a burst of concurrent connections re-mints at most once.
#[derive(Debug)]
pub struct RefreshableCred {
    current: RwLock<CredState>,
    refreshing: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
struct CredState {
    secret: String,
    /// `None` for a static secret (never refreshed).
    expires_at: Option<DateTime<Utc>>,
}

impl RefreshableCred {
    /// Build a cell. `expires_at = None` marks a static secret (never refreshed).
    pub fn new(secret: String, expires_at: Option<DateTime<Utc>>) -> Arc<Self> {
        Arc::new(Self {
            current: RwLock::new(CredState { secret, expires_at }),
            refreshing: tokio::sync::Mutex::new(()),
        })
    }

    fn secret(&self) -> String {
        self.current.read().secret.clone()
    }

    /// Is the credential comfortably inside its validity window? A static secret
    /// (no TTL) is always fresh; a minted one is fresh until 5 min before expiry —
    /// the same window the coordinator's own token cache re-mints on
    /// (`GitHubApp::mint_basic`), so the proxy asks for a re-mint exactly when a
    /// fresh token is actually available.
    fn fresh_enough(&self) -> bool {
        match self.current.read().expires_at {
            None => true,
            Some(exp) => exp > crate::time_source::wall_now() + Duration::minutes(5),
        }
    }
}

/// WS4: the seam the host-agent injects so the proxy can re-mint a near-expiry
/// inject credential via the coordinator (which holds the mint authority). An
/// async trait object — unlike the fire-and-forget [`crate::observe::ObserveSink`]
/// the proxy AWAITS the fresh secret before injecting it.
#[async_trait]
pub trait InjectRefresher: Send + Sync {
    /// Re-mint the credential for `mint_provider` on `session_id`. `None` ⇒ the
    /// refresh failed (the caller keeps the stale secret).
    async fn refresh(&self, session_id: SessionId, mint_provider: &str) -> Option<RefreshedInject>;
}

/// WS4: the result of an [`InjectRefresher::refresh`] — the fresh rendered header
/// value + its new expiry. `header_name` is unchanged across a refresh (same
/// scheme), so it isn't carried here.
#[derive(Clone, Debug)]
pub struct RefreshedInject {
    pub secret: String,
    pub expires_at: DateTime<Utc>,
}

/// ADR 0059: a GraphQL operation type. The body-parsed operation must equal this
/// for a [`GraphqlMatch`] to fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphqlOperation {
    Query,
    Mutation,
    Subscription,
}

impl GraphqlOperation {
    /// Parse the wire token (`"query"` | `"mutation"` | `"subscription"`,
    /// case-insensitive). `None` for anything else (a REST entry leaves the
    /// GraphQL fields empty, so the caller only parses non-empty tokens).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "query" => Some(Self::Query),
            "mutation" => Some(Self::Mutation),
            "subscription" => Some(Self::Subscription),
            _ => None,
        }
    }
}

/// ADR 0059: one GraphQL operation matcher — operation type + top-level field
/// name (e.g. `mutation` / `mergePullRequest`). Field comparison is
/// case-sensitive (GraphQL field names are); operation is the enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphqlMatch {
    pub operation: GraphqlOperation,
    pub field: String,
}

impl GraphqlMatch {
    /// Does this matcher cover a parsed `(operation, field)` selection? The field
    /// is the *underlying* GraphQL field name (aliases are resolved by the parser
    /// before this is called — an alias must never bypass the gate).
    pub fn matches(&self, operation: GraphqlOperation, field: &str) -> bool {
        self.operation == operation && self.field == field
    }
}

/// ADR 0056: the request shapes a Plane-B injection gates + applies to.
/// Layered on top of the SNI host match. `methods` empty = any method;
/// `path_globs` empty = any path.
///
/// `path_globs` are **glob patterns** — a `*` matches any run of characters
/// (including `/`) — matched against the whole request path (the connector op's
/// full `match.path`). So `POST /repos/o/r/pulls` matches `/repos/*/pulls` while
/// `POST /repos/o/r/git/refs` does not. (These were once truncated at the first
/// `*` and prefix-matched — a coarse over-match where `/repos/*/pulls` collapsed
/// to `/repos/` and the gate fired on *any* `/repos/…` request; fixed in #413,
/// renamed from `path_prefixes` to match.)
///
/// ADR 0059: when `graphql` is `Some`, this entry gates a GraphQL operation — the
/// request must still satisfy `methods`/`path_globs` (the `POST /graphql`
/// endpoint), AND the body-parsed top-level operation must match. REST entries
/// leave `graphql` `None`; their `allows(method, path)` semantics are unchanged.
#[derive(Clone, Debug, Default)]
pub struct RequestPolicy {
    pub methods: Vec<String>,
    pub path_globs: Vec<String>,
    pub graphql: Option<GraphqlMatch>,
}

impl RequestPolicy {
    /// Is `(method, path)` permitted by the REST gate? Method match is
    /// case-insensitive; path match is a glob over the whole path (`*` = any
    /// chars). Empty list = "any". This ignores `graphql` — GraphQL entries are
    /// gated by [`Self::method_matches`] + [`Self::path_matches`] **plus** the
    /// body parse (see `intercept`), not by `allows`.
    pub fn allows(&self, method: &str, path: &str) -> bool {
        self.method_matches(method) && self.path_matches(path)
    }

    /// Case-insensitive method match; empty `methods` = any method.
    pub fn method_matches(&self, method: &str) -> bool {
        self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }

    /// Glob path match over the whole path; empty `path_globs` = any path.
    pub fn path_matches(&self, path: &str) -> bool {
        self.path_globs.is_empty() || self.path_globs.iter().any(|p| glob_match(p, path))
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
    /// ADR 0059 (GraphQL): record when the response is HTTP 2xx AND its JSON body
    /// carries no non-empty top-level `errors` array. The `errors` check needs the
    /// body, so [`Self::satisfied_by`] only covers the 2xx half; `observe::evaluate`
    /// applies the `errors` gate for this variant.
    NoGraphqlErrors,
}

impl SuccessRule {
    /// Does `status` satisfy this rule's HTTP-status half? `None` (unparseable
    /// status) is treated as "indeterminate" — see [`crate::observe`] for the
    /// coarse-emit posture. For [`Self::NoGraphqlErrors`] the body-level `errors`
    /// check is applied separately in `observe::evaluate`.
    pub fn satisfied_by(&self, status: u16) -> bool {
        match self {
            Self::StatusClass2xx | Self::NoGraphqlErrors => (200..300).contains(&status),
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
    /// `(field name, extractor path)` pairs, e.g. `("number", "$.resp.number")`
    /// or `("title", "$.vars.input.title")` (GraphQL request variables).
    /// Repeated field names form a fallback chain — the first extractor that
    /// resolves wins (`observe::evaluate`).
    pub data: Vec<(String, String)>,
    /// Extractor path yielding an external URL, e.g. `"$.resp.html_url"`.
    pub fetchable: Option<String>,
    /// Derive `data` fields the extractors missed from the fetchable URL
    /// (GraphQL parity — a response echoes only the client's selection set).
    /// See [`crate::observe::UrlFallback`].
    pub url_fallback: Option<crate::observe::UrlFallback>,
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
        if self.allow_all || self.network_allow.matches(hostname) {
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
    // tests drive a live system; wall clock/OS entropy here is input, not a
    // decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use std::str::FromStr;

    fn state() -> SessionState {
        SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::from_str("10.200.0.2").unwrap(),
            allow_all: false,
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
                header_name: "DD-API-KEY".into(),
                header_template: "{}".into(),
                allow: HostList::from_manifest(&["api.datadoghq.com".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["GET".into()],
                    path_globs: vec!["/api/v2/logs*".into()],
                    graphql: None,
                },
                mint_provider: String::new(),
                cred: RefreshableCred::new("dd-secret".into(), None),
            }],
            observes: vec![ObserveEntry {
                allow: HostList::from_manifest(&["api.github.com".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["POST".into()],
                    path_globs: vec!["/repos/*/issues".into()],
                    graphql: None,
                },
                provider: "github".into(),
                asset_kind: "issue".into(),
                surface: "asset".into(),
                success: SuccessRule::StatusClass2xx,
                data: vec![("number".into(), "$.resp.number".into())],
                fetchable: Some("$.resp.html_url".into()),
                url_fallback: None,
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

    struct StubRefresher {
        calls: std::sync::atomic::AtomicUsize,
        result: Option<RefreshedInject>,
    }

    #[async_trait]
    impl InjectRefresher for StubRefresher {
        async fn refresh(&self, _s: SessionId, _p: &str) -> Option<RefreshedInject> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn mint_entry(secret: &str, expires_at: Option<DateTime<Utc>>) -> InjectEntry {
        InjectEntry {
            header_name: "Authorization".into(),
            header_template: "Bearer {}".into(),
            allow: HostList::from_manifest(&["api.github.com".into()], &[]).unwrap(),
            policy: RequestPolicy::default(),
            mint_provider: "github".into(),
            cred: RefreshableCred::new(secret.into(), expires_at),
        }
    }

    #[tokio::test]
    async fn static_entry_never_refreshes() {
        // Empty mint_provider (a static secret) is a no-op even with a refresher.
        let e = InjectEntry {
            mint_provider: String::new(),
            ..mint_entry("static", None)
        };
        let r = StubRefresher {
            calls: Default::default(),
            result: Some(RefreshedInject {
                secret: "fresh".into(),
                expires_at: Utc::now() + Duration::hours(1),
            }),
        };
        e.refresh_if_stale(SessionId::new(), &r).await;
        assert_eq!(r.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(e.secret(), "static");
    }

    #[tokio::test]
    async fn fresh_minted_entry_is_not_refreshed() {
        // Expiry comfortably beyond the 5-min window → no re-mint.
        let e = mint_entry("current", Some(Utc::now() + Duration::hours(1)));
        let r = StubRefresher {
            calls: Default::default(),
            result: None,
        };
        e.refresh_if_stale(SessionId::new(), &r).await;
        assert_eq!(r.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(e.secret(), "current");
    }

    #[tokio::test]
    async fn near_expiry_minted_entry_refreshes() {
        // Inside the 5-min window → re-mint and adopt the fresh secret.
        let e = mint_entry("stale", Some(Utc::now() + Duration::minutes(2)));
        let r = StubRefresher {
            calls: Default::default(),
            result: Some(RefreshedInject {
                secret: "fresh".into(),
                expires_at: Utc::now() + Duration::hours(1),
            }),
        };
        e.refresh_if_stale(SessionId::new(), &r).await;
        assert_eq!(r.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(e.secret(), "fresh");
        // Now fresh — a second call is a no-op.
        e.refresh_if_stale(SessionId::new(), &r).await;
        assert_eq!(r.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_failure_keeps_stale_secret() {
        // A failed re-mint must NOT drop the credential — a stale token 401s
        // (recoverable) but the request still goes out.
        let e = mint_entry("stale", Some(Utc::now() + Duration::minutes(1)));
        let r = StubRefresher {
            calls: Default::default(),
            result: None,
        };
        e.refresh_if_stale(SessionId::new(), &r).await;
        assert_eq!(r.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(e.secret(), "stale");
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
