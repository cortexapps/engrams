# ADR 0059: GraphQL operation gating + observation for integration connectors

Status: 2026-06-25 — **Accepted** (implementation landed; runtime stack-smoke +
field-set finalization are the remaining operational gates, below). The egress
proxy gains a body-parsing GraphQL parser so a connector power (e.g. `pulls:write`)
authorizes the GraphQL operations that back it, not just the REST endpoints —
closing a regression the ADR 0056 connector model opened for `gh`. Field-level
gating + response observation. All unit/integration tests + `just check` +
orchestrator typecheck/tests are green; the local-stack smoke against a live `gh`
(and the real-traffic refinement of the GitHub query-field set) is the post-merge
validation gate, not a design open. Commit chain at the bottom.

**Implementation divergence from the phasing below:** the proxy engine and the
wire/coordinator/host-agent threading shipped as **one** Rust commit rather than
two — splitting them would have left a non-compiling intermediate (the proxy's new
`RequestPolicy.graphql` field and the host-agent construction site that fills it
are inseparable). The capability is otherwise exactly as phased.

Builds on **ADR 0056** (generic integrations — the interceptor gate/inject/observe
+ connector config; §3 already sketches the GraphQL `match` shape as deferred
future work), **ADR 0057** (profiles as the unified session-policy object),
**ADR 0058** (CLIs as first-class integration consumers — `gh` rides the inject
plane), and **ADR 0006** (the host-agent egress proxy).

## Context

ADR 0056 retired GitHub's bespoke pull-request seam (`ForgeOp::CreatePullRequest`
+ a fixed-scope mint + a hand-emitted PR asset) in favor of a generic, API-first
allowlist. A connector declares `operations`, each tagging a *power* (`grants`,
e.g. `pulls:write`) onto an HTTP `match: { method, path }`. The orchestrator
compiles a profile's bound capabilities into a per-session policy; the host egress
proxy MITMs outbound HTTPS, **gates** each request by `(host, method, path)`,
**injects** the credential (a minted GitHub App token for `github`), and
**observes** responses to emit `IntegrationAsset`s. This is clean and complete for
REST.

It is blind to GraphQL. GitHub's `gh` CLI sends a large fraction of its operations
as GraphQL: a single endpoint `POST https://api.github.com/graphql`, with the real
operation living in the JSON request body
(`{"query":"mutation {...}","variables":...,"operationName":...}`). Method+path
gating cannot tell a read `query` from a destructive `mutation` — every GraphQL
call is `POST /graphql`. The allowlist's only safe response is to **deny `/graphql`
entirely**. So the moment ADR 0056 removed the old forge seam, this regressed:

- `gh pr merge` / `review` / `ready` / `view` / `list`, `gh issue view` / `list`,
  `gh repo view`, `gh search`, `gh api graphql` — all 403 at the proxy, because
  they ride GraphQL and no policy can match `POST /graphql` at the right
  granularity.

The old seam happened to cover the PR-write half via its mediated path; the new
model dropped GraphQL on the floor for every provider, GitHub most visibly.

ADR 0056 §3 anticipated this: it lists GraphQL as a **designed-in future shape**
with `match: { operation, field }`, `success: { noGraphqlErrors: true }`, and
`$.resp.data.<field>...` extractors — explicitly "a later phase adds [it] as a new
host-proxy parser." This ADR is that phase.

## Decision

Teach the egress proxy to **parse the GraphQL request body** and gate by
`(operation, field)`, so the **same power flows to both the REST and GraphQL
surfaces** of a provider. Two sub-decisions:

### 1. Field-level gating, default-deny (not an operation-type tier)

The proxy parses the document's operation type (`query`/`mutation`/`subscription`)
and its **top-level selection-set fields** (resolving aliases to the underlying
field name), and matches each against the connector's granted GraphQL operations.
A request is permitted only if **every** top-level field is covered by some
granted operation; any uncovered field denies the whole request. Unmapped
operations are denied by default — exactly mirroring the REST model, where an
unmapped endpoint 403s.

We rejected a coarser "query=read / mutation=write" tier. It is simpler (no field
map, only operation-type parsing) and for GitHub the minted App token would still
bind server-side — but it is unsafe for **inject** connectors carrying a broad
static token (the proxy gate is their *only* boundary), and it discards the
per-resource precision the powers model exists to provide. Field-level keeps one
uniform, least-privilege semantics across mint and inject connectors.

**Top-level granularity is asymmetric, by GitHub's schema shape, and that is
fine.** GraphQL **mutations** have specific root fields (`mergePullRequest`,
`createIssue`, …) — write-side gating is genuinely precise, and is where the
least-privilege value lands. GraphQL **queries** enter through a handful of root
Query fields (`repository`, `viewer`, `node`, `organization`, `search`) with the
resource nested inside — so gating `query.repository` authorizes reading whatever
the token can see under a repo. For reads, the minted token's `contents:read` /
`pulls:read` permissions are the real scoper and the proxy field-gate is a coarse
first line; for writes, **both** the proxy field-gate and the mint scope bind.

### 2. Gate **and** observe (preserve the side-effect⟹event invariant)

ADR 0056's load-bearing invariant is that *a side effect cannot occur without an
event*. Gating GraphQL mutations while ignoring their responses would reopen the
very gap ADR 0056 closed (an invisible `gh issue create`). So GraphQL operations
carry the same `asset` map as REST ones, observed at the proxy: success is "HTTP
2xx **and** no non-empty top-level `errors` array" (`noGraphqlErrors`), and asset
`data` is extracted via `$.resp.data.<field>...`. A `createIssue` mutation emits an
`issue` asset just like `POST /repos/*/issues`.

### Design refinement over the ADR 0056 sketch: match-shape, not connector-protocol

ADR 0056 §3 sketched GraphQL as a connector-level `protocol: "graphql"` (its
example, Linear, is all-GraphQL). GitHub is **not** single-protocol — it is REST
*and* GraphQL under one provider, one mint, one set of powers. A connector-level
protocol switch cannot express that.

So the connector stays `protocol: "http"` — **GraphQL is HTTP transport** (a POST
to a known path; the auth header injects identically). The **match shape** selects
the gating discipline: an operation whose `match` is `{ method, path }` is REST;
one whose `match` is `{ operation, field }` is GraphQL, gated against the
connector's declared `graphqlEndpoint` (default `/graphql`). One connector mixes
both, its operations sharing `grants`. This is strictly more expressive than the
original sketch and requires no connector to pick a single protocol.

### Fail-closed posture (this is a security boundary)

The proxy buffers the **full** request body (bounded; 256 KiB) before deciding —
it cannot stream-then-revoke. Every ambiguity denies (403, body never forwarded
upstream, so nothing leaks):

- body over the cap, unbounded framing (POST with neither `Content-Length` nor
  chunked), or a `Content-Encoding` we won't decode → deny (can't gate what we
  can't read);
- non-UTF8 / non-JSON / missing `query`; a **batched array** request (GitHub uses
  single ops; arrays multiply the gate surface) → deny;
- GraphQL syntax error; a multi-operation document with no `operationName` to
  disambiguate; a **top-level fragment spread or inline fragment** (resolving
  fragments is attacker-controlled surface — nested spreads, cycles — and `gh`
  never spreads at the root) → deny;
- any top-level field not covered by a granted operation → deny the whole request.

**Parser choice:** `async-graphql-parser` — a strict, all-or-nothing parser with a
light typed AST. We deliberately avoid error-*resilient* parsers (e.g.
`apollo-parser`) that return a partial tree on malformed input: at a security gate,
"parse half and guess" is a smuggling vector. A document parses cleanly into the
typed AST or we reject it.

**Alias resolution is the subtlest bypass.** `a: mergePullRequest(...)` must gate
on `mergePullRequest`, never the alias `a`; otherwise any field hides behind an
alias. Locked by a test.

## What this is not

- Not gRPC. ADR 0056's other future shape stays deferred.
- Not GraphQL-over-GET (query in the URL). GitHub is POST-only; a GET to a GraphQL
  endpoint is denied (fail-closed). Revisit only with a concrete connector need.
- Not a relaxation of the mint scope. The minted GitHub App token is unchanged and
  remains the server-side enforcement; this ADR adds the host-side first line for
  the GraphQL surface.

## Consequences

- The proxy now buffers request bodies for GraphQL-endpoint requests only (REST and
  bypass hosts keep the header-only early-stop). One extra read round-trip for a
  few-KiB document; memory bounded by the 256 KiB cap.
- The connector config gains a `match: { operation, field }` form and a
  `graphqlEndpoint`. New wire fields (`graphql_operation`, `graphql_field`,
  `success_no_graphql_errors`) thread through `IntegrationInject/Observe` →
  `EgressInject/ObserveEntry` → the proxy's `RequestPolicy`. All `#[serde(default)]`
  (resume-safe).
- A strict parser means a legitimate `gh` GraphQL op we *didn't* map is **denied**,
  not allowed — the safe failure direction, but the supported field set must be
  exercised against real `gh` traffic before rollout (see Phasing → verification).
- GitHub's `github.json` grows a GraphQL operation block; the `cli.doc` notes the
  newly-working commands.

## Phasing + commit chain

1. **ADR 0059 Proposed** — `792346d2`.
2. **Egress pipeline (Rust)** — `3bfce923`: `engram-egress-proxy` (`graphql.rs`
   parser via `async-graphql-parser`, `RequestPolicy.graphql` +
   `GraphqlMatch`/`GraphqlOperation`, `SuccessRule::NoGraphqlErrors`, intercept
   body-buffer + set-coverage gate + GraphQL observe firing; proxy unit + e2e
   tests + hakari), the wire fields on `IntegrationInject/Observe` +
   `EgressInject/ObserveEntry` (`engram-core`), `session_boot.rs` copy-through,
   host-agent `register_policy` translation, and `WIRE_VERSION` 4→5
   (`engram-protocol` + golden). (Proxy engine + wire threading merged into one
   commit — see the divergence note in the Status; splitting left a
   non-compiling intermediate.)
3. **Orchestrator compile** — `d90acb3a`: `registry.ts` `GraphqlMatch` union,
   `graphqlEndpoint`, `parseConnector` validation, `compileIntegrationPolicy`
   emission, `buildProviderCatalog` access; `connectors.test.ts` + `tasks.test.ts`.
4. **GitHub connector** — `083ef8ac`: `github.json` GraphQL operations mapped to
   existing powers + `cli.doc`.
5. **ADR 0059 Accepted** — this commit.

### Post-merge pitfall (fixed): the parser rejected `gh`'s named single operations

Prod session `2f25d473` surfaced the "exercise against real `gh` traffic" gate the
hard way: every `gh` GraphQL call (`gh repo view`, `gh pr list`, `gh auth status`)
was rejected as `reason="unparseable or unsupported graphql operation"`. Root
cause: `gh` **names** its single operation (`query RepositoryInfo`, `mutation
CreatePullRequest`) and sends **no** `operationName`; `async-graphql-parser`
classifies a *named* operation as `DocumentOperations::Multiple` even when it's the
only one, and the parser required `operationName` to disambiguate `Multiple` → it
fail-closed on every real `gh` request. The unit tests had only exercised
*anonymous* ops (`{ viewer }`), which parse as `Single`. Fixed: a `Multiple` doc
with exactly one operation is accepted without `operationName`; only a genuinely
2+-operation doc still requires it. Regression tests now use the exact `gh repo
view` / `gh pr create` / `gh pr list` (named op + leading fragment definition)
shapes.

### Post-merge pitfall (fixed): multi-field queries duplicated Authorization

Set coverage returns every inject entry needed to cover a document's top-level
fields. A query containing both `viewer` and `repository` therefore matched two
GitHub entries carrying the same installation token, and the injection loop
emitted two `Authorization` headers. GitHub rejects that malformed credential
shape with `401 Bad credentials`, which broke `gh pr create` even though
single-field GraphQL and REST requests succeeded. Injection now coalesces equal
rendered values by case-insensitive header name, as well as separately minted
credentials from the same provider and rendering template. It fails closed if
static or cross-provider entries resolve the same header name to conflicting
values. Unit and TLS e2e regressions cover both the coalescing and conflict cases.

### Post-merge pitfall (fixed): keep-alive requests bypassed the gate + injection

The interceptor gates, strips, and injects only the FIRST request on an
intercepted TLS connection — after forwarding the rewritten prefix it streams
both directions verbatim (`copy_bidirectional`). A keep-alive client's second
request therefore reached the upstream ungated, still carrying the guest's
placeholder credential; GitHub answers that with `401 Bad credentials`. Every
multi-request `gh` command broke on request #2+ (`gh pr create`'s
existing-PR pre-check, `gh pr checks`' schema feature-detection `__type`
queries, `gh run list`'s workflows+runs pair), while single-request probes
(`gh api`, `gh pr view`, `git push`) worked — which made it masquerade as a
credential/mint bug (the pitfall above) long after that fix shipped. Two
compounding tells from the 2026-07-21 prod diagnosis (session `af28cac4`):
the same failing request alternated between EOF (fresh connection → policy
reject of the uncovered `__type` field) and 401 (reused connection → GitHub
saw the placeholder), and `gh` *caches* the 401 responses to its
feature-detection queries in `~/.cache/gh`, so later invocations kept failing
instantly even on fresh connections. Fixed: the proxy now forces
`Connection: close` (and strips `Keep-Alive`/`Proxy-Connection`/`Upgrade`) on
every intercepted request — the same rewrite the observe path always applied —
so a compliant upstream answers once and closes, the client reconnects, and
every request is gated + injected. This covers the substitution plane too (a
reused connection's second request also skipped placeholder substitution).
`Upgrade` is *stripped, never honored* (PR #846 security review): the header is
guest-supplied, and a REST/GraphQL host that doesn't upgrade would ignore it
and keep the connection persistent — letting a guest reopen the very bypass
this fix closes by decorating request #1 with `Upgrade: websocket` +
`Connection: Upgrade`. No intercepted (credential/observe) host speaks
websockets; genuine websocket support would need an explicit per-policy opt-in
plus a 101-aware tunnel, not trust in a client header. Residual (accepted): an
upstream that *ignores* `Connection: close` leaves the tunnel open for
ungated-but-uncredentialed requests; real API hosts honor it. Unit + TLS e2e
regressions (`keep_alive_second_request_cannot_bypass_the_gate`,
`force_close_strips_guest_supplied_upgrade`) pin the rewrite, the Upgrade
strip, and the one-request-per-connection contract.

### Remaining operational gates (post-merge, not design opens)

- Capture real `gh` GraphQL traffic (`GH_DEBUG=api` / mitmproxy) for the target
  commands and tighten `github.json`'s query-field set to match — the strict
  parser denies an unmapped field, so the supported set must be exercised before a
  prod profile relies on it. (The named-operation pitfall above was the first such
  catch.)
- Local-stack smoke (`just integration-session` / e2e): a `pulls:write` +
  `issues:write` profile runs a GraphQL-backed `gh` write — it succeeds, emits an
  `IntegrationAsset`, an unmapped GraphQL mutation is 403'd, and a read-only
  profile is denied GraphQL mutations.
- The web custom-connector authoring UI gains a GraphQL match form (out of scope
  here — GraphQL connectors are already authorable via raw JSON; the built-in
  GitHub connector, which fixes the regression, needs no UI).

### Verification (gate before Accepted)

- `just check` + orchestrator `bun run typecheck && bun test`.
- Capture real `gh` GraphQL traffic (`GH_DEBUG=api` / mitmproxy) for the target
  commands; confirm each maps to a granted field; tighten `github.json` to match.
- Local stack smoke: a `pulls:write`+`issues:write` profile runs a GraphQL-backed
  `gh` write — it succeeds, emits an asset, and an *unmapped* GraphQL mutation is
  403'd; a read-only profile is denied GraphQL mutations.
