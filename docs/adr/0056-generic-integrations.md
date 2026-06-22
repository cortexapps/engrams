# ADR 0056: Generic third-party integrations — the interceptor is the universal gate + asset observer; credentials are an orthogonal axis

Status: 2026-06-21 — **Proposed.**

Revised 2026-06-21: **reframed around the interceptor as the universal choke
point.** The egress proxy (ADR 0006) gates every outbound request *and* observes
responses to emit assets — for *all* providers. "Credential source" (mint a scoped
token the guest wields, vs. inject a static key at the proxy — the old "Plane A /
Plane B") is now an **orthogonal axis** that only answers *how a request is
authenticated*, not what is allowed or what surfaces. Surfaced assets become a
**declarative property of an API endpoint** in connector config, observed from the
real response bytes at the proxy — so a side effect on a marked endpoint **cannot
occur without emitting an event**. A server-mediated action seam survives only as
a **hybrid exception** for the rare cases needing pre-effect mediation.

Builds on ADR 0023 (the git-forge seam + per-session broker token), ADR 0053
(session profiles — the per-session capability selector), ADR 0055 (dynamic
per-session mounts — skills/connectors ship without platform code), ADR 0006 (the
host-agent TLS-MITM egress proxy — *the* substrate here), and ADR 0047 (the
stateless coordinator — Postgres is the authority).

## Context

Today a session's GitHub access is "PR generation for free." Every forge-bound
session mints a GitHub App installation token with a **hardcoded** scope set —
`{ contents: write, pull_requests: write }` (`engram-git-github/src/lib.rs`) — and
a created PR surfaces through a bespoke `SessionEvent::PullRequestOpened` rendered
by a bespoke React card. GitHub is special-cased end to end, and two structural
problems follow:

1. The whole shape (a provider that hands a session credentials *and* surfaces
   assets) generalizes to nothing — every new provider/action is bespoke plumbing.
2. **Side effects that go through a credential don't surface at all.** If the agent
   uses its token to `gh issue create` or `curl` an API directly, the platform
   never sees it — only the one hand-mediated path (PR creation) emits an event.

We want GitHub to stop being special — to be **one instance of a generic
integration system** — and we want the stronger property that **any side effect on
a configured endpoint is recorded**, no matter how the agent made the call. The
key realization: **all guest egress already flows through one TLS-MITM choke
point** (ADR 0006). That choke point — not a per-provider Rust adapter — is the
natural place to gate requests and observe assets.

## Decision

### 1. Capabilities, and the interceptor as the universal choke point

A **capability** is a `(provider, action, resource)` triple
(`github:issues:write`, `datadog:logs:read`, `s3:read@bucket`). A **profile**
(ADR 0053, orchestrator-owned) carries a set of them; at session create the
orchestrator threads them to the coordinator, which **binds them to the session in
Postgres** in the create transaction.

The **interceptor** (the egress proxy) is the single point every outbound request
flows through. For each intercepted host it does three things, *independent of how
the request is authenticated*:

1. **Gates** the request: allow/deny on `(host, method, path)` against the
   session's bound capabilities + connector config.
2. **Supplies/locates auth**: either passes through a credential the guest already
   holds (minted), or injects a static key it holds host-side (injected) — see §2.
3. **Observes** the response: for endpoints marked *asset-generating* in config, it
   reads the real response and emits an `IntegrationAsset` (§4).

**The load-bearing invariant: a side effect cannot occur without an event.** On a
MITM'd host, a marked endpoint *always* emits — whether the agent used `gh`,
`curl`, an SDK, or raw `fetch`, the bytes physically traverse the choke point. This
is the property the old mediated-seam design could not provide (a credential-path
side effect simply vanished). The honest caveat: this holds for **intercepted
(MITM'd) hosts**; a bypass-relayed host's bodies are unseen, so asset-bearing
providers are configured *always-intercept* (§5).

The guest stays untrusted: it can only *make requests*; the interceptor (and the
broker behind it) decide what is allowed from the PG-bound capabilities, and emit
assets from **observed bytes**, never from guest claims.

### 2. The orthogonal axis: credential *source* (mint vs inject)

Authentication is a separate concern from gating/observation. A capability resolves
to one of two **credential sources**:

- **Mint** (formerly "Plane A"): the broker mints a narrow, short-lived credential
  the **guest wields**, and the *resource server* enforces its scope (GitHub App
  installation token with a computed `permissions` subset; AWS STS; GCP
  impersonation; OAuth token-exchange). The interceptor passes the guest's request
  through (it may *observe*, but must not *modify* — some are signed, e.g. SigV4).
- **Inject** (formerly "Plane B"): the proxy holds a static key host-side and
  **injects** the auth header; the guest never holds the secret (Datadog, Stripe,
  internal `X-API-Key`). Declarative connector config — no Rust.

These are the *only* differences between the two; **gating and asset emission are
identical and always happen at the interceptor.** This is the correction to the
prior framing, which over-loaded "Plane A/B" with enforcement *and* auth. Mint is
hand-coded per provider (bespoke, security-critical — §6/§9); inject is config.

### 3. The connector config — one declarative per-endpoint table

A connector describes a provider as a table keyed by endpoint pattern. It is the
single place that says, per endpoint: *is it allowed, does the proxy inject a key,
and is it an asset?* — plane-agnostic.

```yaml
provider: github
endpoints:
  - match: { method: POST, path: /repos/*/pulls }
    allow: true
    asset:
      kind: pull_request            # → IntegrationAsset{provider:"github", asset_kind:"pull_request"}
      surface: asset
      success: "status:2xx"
      data:      { number: $.resp.number, title: $.resp.title, repo: $.req.path[1:3] }
      fetchable: { external_url: $.resp.html_url }
  - match: { method: POST, path: /repos/*/issues }
    allow: true
    asset: { kind: issue, surface: asset, success: "status:2xx",
             data: { number: $.resp.number, title: $.resp.title },
             fetchable: { external_url: $.resp.html_url } }
  - match: { method: GET, path: /repos/* }       # read-only, allowed, not an asset
    allow: true
```

```yaml
provider: datadog
inject: { header: "DD-API-KEY", secret_ref: "datadog-api-key" }   # credential source = inject
endpoints:
  - match: { method: POST, path: /api/v2/logs/events/search }
    allow: true
    asset: { kind: query_result, surface: action, success: "status:2xx",
             data: { count: $.resp.meta.page.total_count } }
```

The `asset` block is exactly the "munge the response into a renderer-interpretable
payload" surface: it maps request/response fields into the semantic
`IntegrationAsset` the Phase-1 renderer already consumes. **Adding an
asset-generating API on *any* provider — a GitHub issue, a Datadog query, a Linear
issue — is a config row. Zero Rust.** Connectors live in an orchestrator-owned
registry (à la the ADR 0055 catalog) and are compiled into the per-session
`SessionEgressPolicy` shipped to the host at create.

### 4. Assets: observed at the interceptor by default; mediated only as a hybrid exception

Retire `SessionEvent::PullRequestOpened` into one generic event (`FileShared` /
artifacts, ADR 0026, are **not** integration-related and stay as-is; the
`FetchableRef::Artifact` handle remains so a future asset *can* point at the
artifact store). The wire is **semantic-only — no rendering instructions**:

```rust
// engram-coordinator/src/state.rs  (Phase 1 — already landed)
IntegrationAsset {
    provider: String, asset_kind: String,   // "github"/"issue", "datadog"/"query_result"
    surface: AssetSurface,                   // Action (transient) | Asset (durable; survives recovery rewind)
    data: serde_json::Value,                 // the typed semantic payload (from the config `data` map)
    fetchable: Option<FetchableRef>,         // External{url} | Artifact{id,mime,size}
    at: DateTime<Utc>,
}                                            // kind() == "integration_asset"
```

**Two emit paths, observational by default:**

- **Observed (default).** The interceptor reads the *real* response bytes (trusted
  host-side), evaluates the connector's `asset` map, builds the `IntegrationAsset`,
  and sends it to the coordinator to persist + broadcast. Never a guest claim.
  **Robustness rule: emit a *coarse* asset even when parsing fails** (e.g. "a
  `POST /repos/x/issues` returned 201"), enriched by the map on success — so the
  side-effect ⟹ event invariant survives a parser miss.
- **Mediated (hybrid exception).** A thin server-side seam (the slim
  `Integration::perform_action`, §6) survives **only** for actions needing
  *pre-effect* mediation — platform-initiated actions, multi-step orchestration, or
  cases where the guest must never hold even a scoped token. Today's PR creation
  rides this; new actions default to observed.

The event shape **and** the web `(provider, asset_kind) → renderer` registry (both
landed in Phase 1) are unchanged — they are exactly the rendering layer the config
`data`/`fetchable` map feeds. An unknown pair falls back to a generic card (zero
web code); a polished card is an opt-in registry entry. The future WYSIWYG plugin
authors *both* the response→asset map (config) and the asset→pixels treatment
(web/orchestrator) — never the wire event.

This is cheap on the wire because persistence/replay/SSE are already opaque over
`(kind, payload_json)`; the only per-kind coupling is the web + one branch in
`rewind_session_to_cursor` (generalized to `kind='file_shared' OR
(kind='integration_asset' AND payload->>'surface'='asset')`).

### 5. The engine cost (be honest about the investment)

Observation is the real work, and it lives in the one layer everything already
flows through:

- **The proxy must buffer + parse response bodies for marked endpoints.** Today
  `intercept.rs` rewrites the *request* prefix and streams the response blind. We
  add: bounded response buffering (only for marked endpoints — everything else
  still streams), HTTP/1.1 only (already constrained), and **strip `Accept-Encoding`
  on requests to marked endpoints** so responses come back identity (no gzip/br to
  decompress).
- **A small mapping evaluator** (JSONPath-ish) for the `data`/`fetchable`/`success`
  expressions, plus a **host→coordinator emit channel** (the proxy lives in the
  host-agent; it sends the built asset up to the coordinator — mirrors the harness
  event path).
- **Observation is post-hoc**: the asset is recorded *after* the effect (correct
  for an audit — you can't un-create a PR). The *pre-gate* is the request-side
  allow/deny, which runs before forwarding.
- **Limits:** gRPC/protobuf bodies and signed-body APIs (SigV4) are *observe-only*
  (we can read but not inject/modify) — fine, because signed APIs use mint, not
  inject. Observation requires MITM (cert cost); the airtight invariant requires
  asset-bearing hosts be always-intercept (no bypass).
- **Idempotency:** retries / multi-call actions can double-observe; dedup on a
  stable resource id pulled from the response (the system's posture is already
  at-least-once, ADR 0028).

### 6. What's left in Rust: the slim `Integration` trait (mint + a hybrid seam)

With gating, injection, and asset observation all in the interceptor + config, the
per-provider Rust shrinks to the irreducible parts:

```rust
// engram-core/src/traits/integration.rs — replaces traits/git.rs (deleted)
#[async_trait]
pub trait Integration: Send + Sync {
    fn provider(&self) -> &str;

    /// Credential source = MINT. Mint a scoped, short-lived credential the guest
    /// wields, covering EXACTLY `caps` (already clamped by the broker). Bespoke,
    /// security-critical per provider. INJECT-source providers have NO impl —
    /// they are pure connector config (§3).
    async fn mint_credential(&self, caps: &[Capability], hint: &CredentialHint)
        -> Result<ScopedCredential, IntegrationError>;

    /// Hybrid exception: a server-MEDIATED action for the rare pre-effect cases
    /// (§4). Default Unsupported — most actions are observed at the interceptor,
    /// not mediated here.
    async fn perform_action(&self, _cap: &Capability, _args: &serde_json::Value)
        -> Result<ActionResult, IntegrationError> { Err(IntegrationError::Unsupported) }
}

pub enum ScopedCredential {                          // generalizes today's ScopedToken
    Basic  { username: String, password: String, expires_at: DateTime<Utc> },  // git x-access-token/ghs_…
    Bearer { token: String, expires_at: DateTime<Utc> },                        // OAuth/GCP
    AwsSts { access_key_id: String, secret_access_key: String, session_token: String, expires_at: DateTime<Utc> },
}
```

No `plane()` method, no `Proxy(...)` variant — injection is config, not a trait
shape. The **broker** owns `HashMap<provider, Arc<dyn Integration>>`, exposes
`mint(bound, requested, hint)` = `clamp(requested, bound)` then `mint_credential`,
and compiles connector config into the per-session egress policy. `clamp`/`covers`
are pure, unit-testable — the "server decides the scope" invariant in one place.

### 7. Why git folds — it is mostly config now

A PR and an issue are config-marked GitHub endpoints (§3). The **mint** gives the
agent a scoped installation token; the agent calls `api.github.com` (or `git
push`); the **interceptor** gates the call, and **observes** `POST /repos/*/pulls`
/ `POST /repos/*/issues` to emit the asset — regardless of whether the agent used
`gh`, the API, or our `engram-pr` skill. The only hand-coded GitHub Rust left is
the **mint protocol** (App JWT → installation token, scopes computed from caps);
the askpass shim that bridges git's credential-helper protocol stays a **skill**
(ADR 0055), not platform code. GitHub is no longer special — it is one mint adapter
+ a connector config.

### 8. Step 0 finding: broker-mode secret substitution is already wired

The issue framed `SecretMode::Broker` as "half-built." Investigation shows the
opposite: the substitution path is wired end to end, and only the documentation was
stale. `apply_secrets_to_env` (`api/sessions.rs`) inserts placeholders and *used to
log* a `TODO(secrets-broker)` "not yet implemented" warning — **stale**, predating
ADR 0006 wiring the host side: `build_egress_policy` (`session_boot.rs`) builds an
`EgressSecretEntry{ placeholder, real_value, allow_* }`, `start_agent(…, policy)` →
`notify_session_policy` → `egress::register_policy` registers it, and the proxy
`decide(&sni)` → `intercept::run` runs `scan_for_violation` + `substitute`
(covered by `tests/intercept_e2e.rs`). **Step 0** (landed): reconcile the stale
comments and lock the genuinely-untested coupling — the placeholder
`apply_secrets_to_env` writes into the env must equal the `EgressSecretEntry.
placeholder` the proxy substitutes — via a `broker_env_placeholder_matches_egress_entry`
regression test over an extracted pure `egress_secret_entries` helper. Substitution
is host-side, so it runs in the normal Rust lane (not FC-gated). Header injection +
method/path policy + response observation are later phases, not Step 0.

### 9. Extensibility: declarative is the default; hand-coded Rust is the exception

- **Declarative (config, no Rust, no rebuild):** request gating (allow/deny),
  credential **injection**, and **asset marking on any provider** — all live in the
  connector registry. Adding Datadog, marking a new GitHub/Linear endpoint as an
  asset, opening a read path: config rows.
- **Hand-coded Rust (small, audited):** only credential **minting** protocols
  (GitHub App, AWS STS — bespoke + security-critical) and the rare **mediated**
  `perform_action`. A generic/config-driven (or WASM) mint is deferred until ≥2
  hand-coded mint adapters prove the shape.
- **Presentation:** the `(provider, asset_kind) → renderer` registry in
  web/orchestrator; the future WYSIWYG plugin registers render treatment there.

## Phasing (each phase is one PR on its own worktree)

Phases merge in order; this ADR is updated between them and flipped to **Accepted**
at the end with the commit chain.

0. **ADR + Step 0 broker-secret reconciliation** — *done* (#368).
1. **Generic `IntegrationAsset` event + web `(provider, asset_kind)` renderer
   registry** — *done* (#369). Retire `PullRequestOpened`; generic fallback card;
   `forge/pull_request` reuses today's card. No proto/orchestrator change.
   **Survives the reframe unchanged** — it is the rendering layer the config feeds,
   and the coordinator-side PR emit is the mediated hybrid exception.
2. **Capability model scaffolding.** `Capability` type (+ `parse`/`covers`/`clamp`);
   `session.proto capabilities`; `session_capabilities` PG table + `MetadataStore`
   bind/get; orchestrator `profiles.capabilities`. Bound but not yet enforced.
3. **Interceptor request-gating + credential injection.** The connector config's
   `allow` + `inject`: proxy `Decision` gains method/path enforcement + header
   injection; `EgressSecretEntry` gains an injection variant + `RequestPolicy`;
   `build_egress_policy` compiles bound caps → policy. Datadog reachable, gated, and
   the guest holds no key. (This is the universal-gate substrate.)
4. **Interceptor response-observation + asset specs (the new core).** The connector
   config's `asset`: the proxy buffers + parses responses for marked endpoints,
   evaluates the map (+ coarse-emit-on-failure), and emits `IntegrationAsset` via a
   host→coordinator channel. A marked endpoint on *any* provider now surfaces an
   asset with zero Rust. First consumers: a Datadog query result; a GitHub issue.
5. **Mint (credential source) for GitHub + retire `GitForge`.** Slim `Integration`
   trait (mint + provider metadata + the hybrid `perform_action`); `state.forge` →
   `state.integrations`; `ForgeOp` → a generic `IntegrationOp`; profile-scoped App
   `permissions` computed from caps (default profile keeps today's scopes — no
   regression); one-time App union re-consent; retire `GitConfig`/`[git]`. Decide
   per case whether PR creation stays mediated or moves to observed. Flip Accepted.

## Consequences and risks

- **Response-parsing is a real engine investment** (encodings, buffering, a mapping
  DSL, a host→coordinator emit channel) — but spent in the one layer all egress
  already flows through.
- **Observation is post-hoc + MITM-bound:** assets record after the effect; the
  invariant holds for intercepted hosts, so asset-bearing providers are
  always-intercept. gRPC/signed-body APIs are observe-only.
- **The scope invariant** lives in `clamp`/`covers` (mint) + the connector
  allow/deny (gate); bound caps come from PG, written by the create transaction. A
  guest asking for more gets a clamp-drop / deny.
- **One-time GitHub App re-consent** to the union ceiling; the default-profile-keeps-
  today's-scopes design makes it additive.
- **Resume/snapshot:** broker token + `session_capabilities` rows are session-keyed
  (survive resume + replica hops, ADR 0047); connector-derived egress policy is
  rebuilt on resume by the existing `build_egress_policy` + `notify_session_policy`.
- **Idempotency:** dedup observed assets on a response-derived resource id
  (at-least-once posture, ADR 0028).
- **Backwards-incompatible wire/schema changes** are acceptable — 0 users; clean
  breaks preferred.

## Alternatives considered

- **Mediated-only (a `perform_action` Rust arm per action).** Rejected as the
  *default*: every action is plumbing on three sides, and — fatally —
  credential-path side effects (`gh issue create`) never surface. Kept only as the
  hybrid exception for pre-effect mediation.
- **Carry a render descriptor on the wire.** Rejected: couples the coordinator to
  presentation and blocks the WYSIWYG layer; rendering belongs in web/orchestrator.
- **Let the guest self-report assets.** Rejected: a compromised agent could forge an
  asset. The interceptor reads *real bytes* — it is the trusted observer.
- **Over-load "Plane A/B" with both auth and enforcement** (the prior framing).
  Rejected: gating + observation are universal (the interceptor); only the
  credential *source* differs. Splitting the axes is what makes assets declarative.
- **Generic/config-driven (or WASM) mint so *all* providers are codeless.**
  Deferred (§9): minting is bespoke + security-critical; config covers gating,
  injection, and assets, which is most of the surface.
