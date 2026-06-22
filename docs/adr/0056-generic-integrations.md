# ADR 0056: Generic third-party integrations — credentials + assets/actions; GitHub PRs become one adapter

Status: 2026-06-21 — **Proposed.** Phase 0 (this ADR + the Step 0 broker-secret
reconciliation) lands first; the remaining phases follow as separate PRs and
update this ADR between them. Builds on ADR 0023 (the git-forge seam + per-session
broker token), ADR 0053 (session profiles — the per-session capability selector,
which named capability scoping as its "natural future home"), ADR 0055 (dynamic
per-session mounts — skills ship without platform code), ADR 0006 (the host-agent
TLS-MITM egress proxy), and ADR 0047 (the stateless coordinator — Postgres is the
authority).

## Context

Today a session's GitHub access is "PR generation for free." Every forge-bound
session mints a GitHub App installation token with a **hardcoded** scope set —
`{ contents: write, pull_requests: write }` (`engram-git-github/src/lib.rs`) — and
a created PR surfaces through a bespoke `SessionEvent::PullRequestOpened` rendered
by a bespoke React card. GitHub is special-cased end to end: the scope is not
least-privilege, it is not declared per-session, and the whole shape (a provider
that hands a session credentials *and* surfaces assets/actions) generalizes to
nothing.

We want GitHub to stop being special — to be **one instance of a generic
third-party integration system** in which any provider can:

1. **Provide scoped credentials** to a session, declared least-privilege at the
   **profile** level (ADR 0053 profiles are already the per-session capability
   selector); and
2. **Surface special assets/actions** into a session — exactly the way an opened
   PR does today — without bespoke platform plumbing or per-provider web code.

The outcome: adding Datadog, Linear, S3, GCS, an internal API, etc. is "write an
adapter + ship a skill," the same low-friction property ADR 0055 gives skills.
Both capabilities are two faces of one `Integration` abstraction, and `GitForge`
is retired into it (clean break — 0 users; the repo prefers this over compat
shims).

## Decision

### 1. The model: profiles declare capabilities; a broker resolves each to the narrowest enforceable form

A **capability** is a `(provider, action, resource)` triple, written
`provider:action[@resource]`:

```
github:contents:write@cortexapps/engrams
github:pulls:write
datadog:logs:read
s3:read@my-bucket
```

A **profile** (ADR 0053, orchestrator-owned) carries a set of capabilities, just
as it already carries `selected_skills`. At session create the orchestrator threads
the profile's capabilities to the coordinator, which **binds them to the session
in Postgres** inside the create transaction. From then on the **broker** (a
coordinator component) is the only thing that turns a capability into access, and
it **clamps** every guest request to the bound set.

**The invariant (carried over from ADR 0023, made explicit):** the guest is
untrusted. It can only *ask* — "give me a credential for X" / "perform action Y."
The broker *decides* what is allowed from the PG-bound profile capabilities and
narrows to it. The guest can never request more than its profile grants. Today the
in-guest forge mint is already authorized by the broker token but its scopes are
chosen server-side; this generalizes that "server decides the scope" property.

A capability compiles into one of two **enforcement planes** (the broker prefers
A):

- **Plane A — credential-side (mint).** Mint a narrow, short-lived credential the
  *resource server* enforces, so it survives any request shape. GitHub App
  installation tokens with a *computed* `permissions` subset (today's hardcode
  becomes "compute from the bound caps"); later AWS STS `AssumeRole` + session
  policy, GCP service-account impersonation, OAuth token-exchange (RFC 8693),
  Vault dynamic secrets.
- **Plane B — egress-proxy-side (inject).** For providers with only a static broad
  key and no per-session downscoping (Datadog, Stripe, internal `X-API-Key`
  services). The egress proxy (already a TLS-terminating MITM, ADR 0006) holds the
  key host-side, injects the auth header on outbound requests, and enforces a
  per-profile **request policy** (`host + method + path`). The guest never holds
  the secret — strictly better hygiene than today's `secret_mode = "literal"`.
  Plane B providers are **declarative** — registered as connector-definition
  config, not Rust (§7).

Two unifications make this exact:

- **Every granted capability also opens that provider's egress host(s)** for the
  session, regardless of plane. The proxy gates *which host* for Plane A too:
  granting `github:*` both mints the token and allows `github.com` egress. Shared
  base = the egress allow-list; Plane A adds *mint*, Plane B adds *inject + request
  policy*.
- **The plane is a property of the adapter, not the provider.** An adapter
  declares `plane()`; moving a provider between planes changes nothing else.

### 2. The `Integration` trait (subsumes `GitForge`)

`GitForge` has two methods that are two different kinds of capability use:
`mint_installation_token` hands the guest a credential it wields directly;
`create_pull_request` performs a server-side action that surfaces an asset. The
generalization keeps them as two methods on **one trait per provider** (a provider
shares one long-lived secret), and adds a *declaration* surface for Plane B:

```rust
// engram-core/src/traits/integration.rs — replaces traits/git.rs (deleted)
#[async_trait]
pub trait Integration: Send + Sync {
    fn provider(&self) -> &str;                 // "github", "datadog", … (matches Capability::provider)
    fn plane(&self) -> Plane;                    // Credential | Proxy(ProxyInjection)

    /// Plane A. Mint a credential covering EXACTLY `caps` (already
    /// clamped by the broker). Plane-B adapters return Unsupported.
    /// Generalizes mint_installation_token: scopes are an argument now.
    async fn mint_credential(&self, caps: &[Capability], hint: &CredentialHint)
        -> Result<ScopedCredential, IntegrationError> { Err(IntegrationError::Unsupported) }

    /// Perform a server-side action (open a PR, post to Slack…). The
    /// result carries the wire reply AND an optional asset the seam
    /// emits onto the event stream. Generalizes create_pull_request.
    async fn perform_action(&self, cap: &Capability, args: &serde_json::Value)
        -> Result<ActionResult, IntegrationError> { Err(IntegrationError::Unsupported) }
}

pub enum Plane { Credential, Proxy(ProxyInjection) } // {secret, header_name, header_template, RequestPolicy}
pub enum ScopedCredential {                          // generalizes ScopedToken
    Basic  { username: String, password: String, expires_at: DateTime<Utc> }, // git x-access-token/ghs_…
    Bearer { token: String, expires_at: DateTime<Utc> },                       // OAuth/GCP
    AwsSts { access_key_id: String, secret_access_key: String, session_token: String, expires_at: DateTime<Utc> },
}
pub struct ActionResult { pub reply: serde_json::Value, pub asset: Option<SessionAsset> }
```

The **broker** owns `HashMap<provider, Arc<dyn Integration>>` and is the only place
the guest's request meets the bound capabilities:

- `mint(bound, requested, hint)` → `clamp(requested, bound)` (set-∩ with
  resource-glob narrowing), then `mint_credential`.
- `act(bound, cap, args)` → reject unless `bound` covers `cap`, else
  `perform_action` and emit the returned asset.
- `proxy_entries(bound)` → at create, compile each Plane-B capability into an
  egress injection entry + request policy.

`clamp` / `covers` are pure, unit-testable functions — the entire "server decides"
invariant in one place.

### 3. Capabilities bind to the profile and the session

- **Orchestrator (profile):** `profiles.capabilities jsonb` (string[]) + a
  `profile.proto` field; `CreateTask` passes it verbatim into
  `CreateSessionRequest.capabilities`, exactly as `selected_skills` is passed.
  Profile-save validates each capability parses and names an advertised provider.
- **Coordinator (session):** `CreateSessionRequest.capabilities` (proto), parsed +
  validated (a bad capability or unknown provider is a create-time 400, mirroring
  the skills cap check), then written to a new `session_capabilities(session_id,
  provider, action, resource)` table inside the create transaction. The broker
  reads these to clamp. Because they are session-keyed PG rows they survive resume,
  replica hops (ADR 0047), and the queued-re-prepare path.
- **The image-level `[git]` block (`GitConfig`) is absorbed.** Provider access is
  no longer an image property; it is a profile capability. `[git]`'s `owner`
  becomes a capability `resource`/`hint` (per-request installation selection,
  answering the multi-install question ADR 0023 left open). This is the governance
  shift ADR 0053 anticipated.

### 4. Assets/actions: one generic event, semantic-only on the wire

Retire `SessionEvent::PullRequestOpened` into one generic event. (`FileShared`
/ artifacts — ADR 0026 — are **not** integration-related and stay as-is; the
`FetchableRef::Artifact` handle remains available so a *future* integration asset
can point at the artifact store, but the file-share event is not migrated.)

```rust
// engram-coordinator/src/state.rs — replaces PullRequestOpened. Semantic ONLY.
IntegrationAsset {
    provider: String, asset_kind: String,   // "forge"/"pull_request", "datadog"/"query_result"
    surface: AssetSurface,                   // Action (verb/transient) | Asset (durable; survives recovery rewind)
    data: serde_json::Value,                 // the typed semantic payload
    fetchable: Option<FetchableRef>,         // External{url} | Artifact{id,mime,size}
    at: DateTime<Utc>,
}
// kind() collapses to "integration_asset".
```

**The wire carries no rendering instructions.** The coordinator is the source of
*what happened*, never *how to draw it*. Rendering lives entirely in the
web/orchestrator layer, keyed on `(provider, asset_kind)`:

- **Now:** the web owns a small `(provider, asset_kind) → renderer` registry.
  engrams-provided shapes (`forge/pull_request`, later `datadog/query_result`) are
  hand-crafted React; an unknown pair falls back to a **generic card** (label =
  `provider/asset_kind`, `data` as a key-value/JSON summary + `fetchable`
  link/media), so a new integration always surfaces *something* sensible with no
  web code. A *polished* card is an opt-in registry entry.
- **Future:** a plugin system (e.g. a "Notion plugin" with a WYSIWYG editor for its
  payloads) registers render treatment in the orchestrator/web plugin layer — still
  without touching the wire event.

This is cheap because persistence/replay/SSE are already opaque over `(kind,
payload_json)` (proto `SessionEvent{idx,kind,payload_json}`; orchestrator forwards
any `kind`). The only per-kind enumeration is in the web (`buildMessages.ts`,
`sse.ts`) plus one coupling in `rewind_session_to_cursor` (ADR 0028 recovery
side-effects), which generalizes to `kind='file_shared' OR (kind='integration_asset'
AND payload->>'surface'='asset')` with a minimal generic side-effect string. The
`SessionState` lifecycle decoder (`parse_session_state`) is unrelated and untouched.

**Emit point:** always coordinator-side at the broker, when the verified action
completes — never a guest self-report of arbitrary metadata. The guest is
authorized (broker token) only to *invoke* the brokered op; the asset is minted
from the op's verified result, the same trust posture as today.

### 5. Why git forge folds — it is two capabilities, not a special case

`GitForge` decomposes cleanly:

- `create_pull_request` → `perform_action` for `github:pulls:write`, returning
  `ActionResult{ asset: Some(SessionAsset::PullRequest{..}) }` → `IntegrationAsset`.
- `mint_installation_token` → **Plane A** `mint_credential` returning today's exact
  `ScopedToken` as `ScopedCredential::Basic{ username:"x-access-token",
  password:"ghs_…" }`.

The **only** git-specific part is *guest-side consumption*: `git` demands
credentials through its `GIT_ASKPASS` / git-credential-helper protocol rather than
an `Authorization` header. A **skill** bridges that protocol to the broker seam
(the `engram-agentd forge-credential` askpass helper + the `/etc/gitconfig` stanza
written by `engram-session-bundles::activate` — already how it works). That is an
integration's guest adapter shipped as a skill (ADR 0055), not platform
specialness. GitHub is Plane A today (the guest transiently holds a short-lived
scoped token — the Plane A tradeoff); it *could* later flip to Plane B (the proxy
injects git auth so the guest holds nothing) by flipping `plane()` alone, with zero
change to the capability or asset model. That swap-ability is the proof forge isn't
special.

### 6. Step 0 finding: broker-mode secret substitution is already wired

The issue framed `SecretMode::Broker` as "half-built." Investigation shows the
opposite: the substitution path is wired end to end, and only the documentation is
stale.

- `apply_secrets_to_env` (`api/sessions.rs`) inserts per-secret placeholders in
  Broker mode **and logs a `TODO(secrets-broker)` warning that the proxy "is not
  yet implemented."** That warning is **stale** — it predates ADR 0006 wiring the
  host side.
- `build_egress_policy` (`session_boot.rs`) builds an `EgressSecretEntry{
  placeholder, real_value, allow_hosts, allow_host_patterns }` for every resolved
  secret and ships it via `start_agent(…, policy)` →
  `HostClient::notify_session_policy` (applied *before* the agent spawns, ADR 0013).
- `PooledBackend::notify_session_policy` → `egress::register_policy` translates it
  into the proxy `Registry` (`SecretEntry`).
- The proxy (`proxy.rs`) looks the session up by guest IP, `decide(&sni)` returns
  `Intercept`, and `intercept::run` runs `scan_for_violation` + `substitute`
  (`substitute.rs`) — both covered by `tests/intercept_e2e.rs` (substitute on an
  allowed host; violation-close on a disallowed host) plus unit tests.

The genuinely untested link is the **coordinator-side coupling**: the placeholder
`apply_secrets_to_env` puts in the guest env must equal the `EgressSecretEntry.
placeholder` the proxy substitutes (they share it only by construction —
`build_egress_policy` reads the placeholder back out of the env). **Step 0**:
reconcile the stale comments and add a regression test locking that invariant (a
pure `egress_secret_entries(bundle, spec_env)` helper extracted from
`build_egress_policy`, asserted against `apply_secrets_to_env` output for both
Broker and Literal modes). No Firecracker VM is required — substitution is entirely
host-side — so the test runs in the normal Rust lane, not an FC-gated lane (a
divergence from the issue's "FC e2e" suggestion, justified by this finding).
Header injection + `method/path` request policy are **out of Step 0** — they are
the real Plane-B gaps, built in Phase 5.

### 7. Extensibility: Plane B is declarative (no Rust); Plane A stays hand-coded

The `Integration` trait is the host-side seam, **not** the unit of extensibility.
How a new provider is added differs sharply by plane, and that asymmetry is a
deliberate goal of this ADR.

**Plane B integrations are declarative — a config row, no Rust, no coordinator
rebuild.** The egress proxy is already a generic, data-driven engine
(`SecretEntry` + the request policy are *data*, ADR 0006). A Plane B provider is
fully described by `(host, injection header name + template, secret reference,
request policy {methods, path prefixes})` — there is no provider-specific code to
write. So Plane B providers are registered as **connector definitions**: rows in
an orchestrator-owned registry, the same shape and spirit as the ADR 0055 skills
catalog and the ADR 0051 connections store, which the broker compiles into the one
generic proxy-inject adapter at session create. Adding Datadog, Stripe, or an
internal `X-API-Key` service is an insert, not a deploy:

    name        text   -- provider key ("datadog", "acme-internal")
    plane       text   -- "proxy" (Plane B)
    config      jsonb  -- { host, header_name, header_template, methods[], path_prefixes[] }
    secret_ref  text   -- pointer into the SecretStore for the static key

This is an explicit deliverable — Phase 5 ships *the generic proxy-inject adapter
+ the connector registry*, with Datadog as the first connector **config** (not a
bespoke Rust impl).

**Plane A stays hand-coded Rust, per provider.** Minting a scoped credential is
provider-specific, security-critical protocol work (GitHub App: JWT signing → an
installation token with a permissions object; AWS STS: SigV4 + AssumeRole session
policy). These are compiled-in `Integration` impls, deliberately — we do not want
user-supplied code holding an App private key. A *generic, config-driven* Plane A
(e.g. one OAuth2 token-exchange adapter parameterized by endpoint + scope map) is
plausible for standard protocols, but the long tail of bespoke mint protocols
realistically needs sandboxed custom logic (**WASM**) or an external mint-RPC the
deployment runs — so generic/pluggable Plane A is **explicitly out of scope here**
and deferred until two or more hand-coded Plane A adapters have proven the trait
shape. Until then, a new Plane A provider = a new (small, audited) Rust adapter.

**Presentation is already above Rust** (§4): the `(provider, asset_kind) →
renderer` registry lives in web/orchestrator, so a connector can ship a card with
no coordinator code.

## Phasing (each phase is one PR on its own worktree)

Phases merge in order; this ADR is updated between them with divergences/pitfalls
and flipped to **Accepted** at the end with the commit chain.

0. **ADR + Step 0 broker-secret reconciliation** (this PR). Reconcile the stale
   `apply_secrets_to_env` comments; add the placeholder-invariant regression test.
1. **Generic `IntegrationAsset` event + web rendering.** Retire only
   `PullRequestOpened`; `(provider, asset_kind) → renderer` registry + generic
   fallback; `forge/pull_request` reuses today's PR card. No proto/orchestrator
   change. `FileShared` untouched.
2. **Capability model scaffolding.** `Capability` type (+ `parse`/`covers`/`clamp`
   + tests); `session.proto capabilities`; `session_capabilities` PG table +
   `MetadataStore` bind/get; orchestrator `profiles.capabilities`. Bound but not
   yet enforced.
3. **`Integration` trait — retire `GitForge`** (pure refactor). GitHub adapter
   implements it; `state.forge` → `state.integrations` broker; `ForgeOp` →
   `IntegrationOp`; PR action emits `IntegrationAsset`; retire `GitConfig`/`[git]`.
   Behavior unchanged (scopes still effectively the default).
4. **Plane A — profile-scoped GitHub.** Broker clamps requested→bound;
   `mint_credential` computes the App `permissions`/`repositories` from caps; the
   default profile keeps today's scopes (no regression); gh-shim consumer; one-time
   App union re-consent.
5. **Plane B — generic proxy-inject adapter + declarative connector registry (§7).**
   `EgressSecretEntry` gains an injection variant + `RequestPolicy`; proxy
   `Decision::Inject` + request-line method/path enforcement + header injection;
   **one** generic proxy-inject adapter (no per-provider Rust); an
   orchestrator-owned **connector registry** whose rows the broker compiles into
   Plane-B entries at create. Datadog is the first connector **config**, not a
   bespoke crate. Flip this ADR to Accepted.

## Consequences and risks

- **One-time GitHub App re-consent** to the union ceiling of every scope any
  profile will request. The default-profile-keeps-today's-scopes design (Phase 4)
  makes this additive, not a cutover.
- **MITM cost / cert-pinned clients** (Plane B): intercept only where we inject;
  cert-pinned SDKs must use Plane A or are unreachable (we control the image).
- **The invariant** lives in `clamp`/`covers` + `proxy_entries`; bound caps come
  from PG, written by the create transaction from the orchestrator-resolved
  profile. A guest asking for more gets a clamp-drop or `NotGranted`. Unit tests on
  these pure functions are the load-bearing proof.
- **Resume/snapshot:** the broker token already survives resume (ADR 0047);
  `session_capabilities` rows are session-keyed (survive resume + replica hops);
  Plane-B entries are rebuilt by the existing `build_egress_policy` +
  `notify_session_policy` on resume. Phase 2 must confirm the queued-re-prepare
  path reads the capability rows.
- **Backwards-incompatible wire/schema changes** (`ForgeOp`→`IntegrationOp`,
  `PullRequestOpened`→`IntegrationAsset`, `[git]` removal, `GitForge` deletion) are
  all acceptable — 0 users; clean breaks preferred (CLAUDE.md).

## Alternatives considered

- **Keep `GitForge` and add integrations alongside it.** Rejected: violates
  "a new abstraction should subsume existing surfaces, not sit alongside them."
- **Carry a render descriptor on the wire** (primitive + label + template).
  Rejected: couples the coordinator to presentation and blocks a future plugin
  WYSIWYG layer; rendering belongs in web/orchestrator.
- **Let the guest self-report assets.** Rejected: a compromised agent could forge a
  PR card or a fake query result. The broker is the trusted emit point.
- **Unify `mint` and `act` into one `invoke(capability, args)`.** Rejected: a
  credential response (a secret the guest wields) and an action response (a
  structured asset the seam emits) have different trust shapes; collapsing them
  loses the distinction the seam needs.
- **A generic, config-driven (or WASM) Plane A so *all* providers are pluggable
  without Rust.** Deferred (§7). Plane B generalizes to pure config because the
  proxy is already a generic engine; Plane A minting is bespoke per protocol and
  security-critical, so it stays hand-coded until two adapters prove the shape —
  at which point WASM or an external mint-RPC is the escape hatch, not core Rust.
