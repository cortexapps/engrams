# ADR 0118: Session apps — named ports, reserved hostnames, and one login wall

- Status: Proposed (2026-08-14)
- Date: 2026-08-14
- Supersedes: ADR 0064 (live-host guest ports at vanity subdomains)
- Related: ADR 0066 (the vsock port relay this keeps and reuses), ADR 0006 (the
  egress proxy that becomes the app-to-app path), ADR 0031 / ADR 0086 (the user
  auth that becomes the wall), ADR 0111 (the durable egress-policy delivery this
  rides), ADR 0051 (the orchestrator is the only web backend)

## Context

ADR 0064 gave a session a **port exposure**: a random tri-word slug that maps to
one `(session, port)` pair, served at `<slug>.<preview base domain>`. ADR 0066
then made the guest hop work for a server bound to `127.0.0.1`. A single-service
dev server works well today.

A multi-service app does not. Three things block it.

**1. An app cannot learn where its sibling is.** A frontend is configured to call
its backend at `localhost:8080`. Inside the guest that address is correct. In the
user's browser it is not — `localhost` is the user's own machine. Nothing in the
platform tells the frontend the backend's public address, and nothing can, because
the exposure is minted *after* the session is live
(`orchestrator/src/rpc/task-create.ts:1171-1190`). The name does not exist when the
guest starts, so it can never become an environment variable.

The edge cannot supply the missing address either. It sends `Host: localhost:<port>`
upstream (`orchestrator/src/routes/preview-proxy.ts:294`) — which is what keeps dev
servers working, since they reject an unknown `Host` outright — so an app that derives
its own public address from the request sees `localhost` too.

**2. The wall is the wrong shape.** IAP fronts the preview Gateway on the deployment
that runs it. It keeps the public internet out, which is correct and must stay true.
But IAP challenges each hostname with a redirect to its identity provider. A browser
can follow that redirect for a top-level navigation. A cross-origin `fetch()` from one
app's page to a sibling app's origin cannot: the browser sees a redirect to a foreign
origin and CORS rejects it. A call made from inside the guest carries no IAP assertion
at all.

**3. Reservation is slow.** The auto-mint loop runs after `createSession` returns
and costs three serial round trips per port
(`orchestrator/src/db/port-exposures.ts:94-121`: SELECT, INSERT, SELECT).

## Decision

Retire the **exposed port**. Add the **session app**.

```
app { name, port }
```

The platform reserves one hostname per app **before it creates the session**, gives
the guest every app's address as environment variables, puts every app URL behind the
same login users already hit, and lets two apps in one session reach each other
without leaving the host machine.

### Every app learns every peer's address

Roughly half the addresses a service needs are a *peer's*, not its own:

| Kind of setting | Lives on | Wants the address of |
| --- | --- | --- |
| CORS allowed origins | the API | the frontend |
| Post-login / callback redirect target | the API | the frontend |
| Cookie domain, OAuth redirect URI | the API | itself |
| HTTP client base URL | the frontend | the API |

An app therefore cannot be given only its own address; the first two rows, which are
most of the work, would have no way to be filled in.

So the platform injects **both forms of every app's address, for every app**, and the
profile remaps them into whatever names its services actually read. This is
Release.com's `<SERVICE>_INGRESS_HOST` / `<SERVICE>_INGRESS_URL` model, and the split
is the point: the bare host is what a CORS allowlist wants, the `https://` form is
what an HTTP base URL wants.

```
apps: [{ name: web, port: 3000 }, { name: api, port: 8080 }]

auto-injected into every process agentd spawns:
  WEB_INGRESS_HOST = web-tidy-swift-otters.preview.example.com
  WEB_INGRESS_URL  = https://web-tidy-swift-otters.preview.example.com
  API_INGRESS_HOST = api-tidy-swift-otters.preview.example.com
  API_INGRESS_URL  = https://api-tidy-swift-otters.preview.example.com

profile env_vars gain `${…}` interpolation over those names:
  CORS_ALLOWED_ORIGINS: ${WEB_INGRESS_URL}   # a peer's address
  FRONTEND_BASE_URL:    ${WEB_INGRESS_URL}   # a peer's address
  API_BASE_URL:         ${API_INGRESS_URL}
```

The env-var map already flows to the guest, so remapping needs no new plumbing. A
`${NAME}` that names nothing we injected is left **verbatim** — env values legitimately
contain shell syntax — but a reference shaped like `${*_INGRESS_*}` that resolves to
nothing is logged, because that shape is always a typo.

### The hostname

`<app-name>-<session-slug>` — one DNS label under the preview base domain. The
session draws one random tri-word slug (`orchestrator/src/apps/hostname.ts`); every app
in that session shares it. The label is readable, it is a single label so the
existing wildcard certificate covers it, and it still does not encode the port. The
whole label is the routing key, so the platform never parses it back into parts.

**Invariant — every app of a session is a subdomain of one registrable domain.**
A very common session-cookie posture is host-only with `SameSite=Lax` and no explicit
override, which is the framework default in several stacks. Such a cookie survives a
cross-origin call only when the two origins are *same-site* — that is, when they share
a registrable domain. Preview hostnames under one base domain preserve that. Splitting
a session's apps across registrable domains would drop the cookie on every cross-app
request, and no CORS configuration could repair it.

### Reservation costs nothing

The reservation adds no round trip anywhere. It is faster than what it replaces.

| Leg | Cost | How |
| --- | --- | --- |
| Mint N hostnames | none | local `crypto.getRandomValues` |
| Persist N rows | none | one batched insert folded into the transaction that already writes `task` and `task_session` (`task-create.ts:1094-1132`). The session UUID is already minted at `:1069` as `requested_session_id`. |
| Give the guest its env | none | fold into `harness_env`, which the coordinator folds into `session_env` (`crates/engram-coordinator/src/api/sessions.rs:1423-1441`), which rides the `SpawnHarness` frame `start_agent` already sends |
| Coordinator persistence | none | a new field on `RuntimeSpec`, written inside the transaction that already runs (`crates/engram-postgres/src/lib.rs:1196`) and re-read on a queued create and on every resume |
| Reach the host | none | rides the `SessionEgressPolicy` notify, already ordered before `start_agent` (ADR 0111) |
| Guest | none | no agentd change, no image re-bake |

agentd applies `session_env` to **every** process it spawns
(`crates/engram-agentd/src/harness_supervisor.rs:112-117`), so the harness, `/exec`,
the shell, and the IDE all see the addresses.

### The wall moves from IAP to the orchestrator

Turn IAP **off** on the preview Gateway. The orchestrator becomes the wall, using
the better-auth session it already owns.

- Widen the session cookie to the domain the main host and the preview base domain
  share (e.g. `.example.com` for `app.example.com` + `*.preview.example.com`). One login
  then covers the main host and every app URL. A sibling `fetch()` carries the
  cookie with no redirect, because the two app origins are the same site.
- An unauthenticated browser navigation gets a 302 to the main host's login page.
  That host stays IAP-gated, so IAP still challenges the human. The bridge mints the
  cookie, and the page returns the user to the app URL.
- An unauthenticated non-browser request gets a 401. Never redirect an `XHR` into an
  HTML login page.
- Default audience is any authenticated principal (`visibility = "org"`). An owner
  may set an app to `private` (owner and admin only). Unauthenticated share tokens
  are retired: the requirement is that nobody reaches an app without passing the
  login wall.

**Invariant — `OPTIONS` skips the wall.** A browser **never** sends cookies on a CORS
preflight. An auth wall in front of per-app hostnames therefore rejects every
preflight before the app sees it, and no amount of CORS configuration in the app can
repair it. This is not a corner case; it is the recurring failure of this exact
architecture:

- Cloudflare Access documents it verbatim — *"the OPTIONS request will return a 403 …
  This is because the browser never includes cookies with OPTIONS requests, by
  design"* — and ships a **"Bypass OPTIONS requests to origin"** toggle.
- Vercel shipped an **OPTIONS Allowlist** for it: *"browsers do not send
  authentication on preflight requests."*
- GitHub Codespaces 302s the preflight to its login page; the browser rejects the
  redirect. Open and staff-acknowledged as a *"known issue"* since November 2022.
- Gitpod returns 401 for the same reason and **closed the issue as not planned**.

So the edge forwards `OPTIONS` to the guest **without requiring a session**, and the
app answers with its own CORS configuration. The app stays authoritative — nothing is
injected, so nothing can duplicate or contradict what the app already emits, which
matters because a framework that supports wildcard origin *patterns* is usually
already correct once its allowlist variable is filled in. The exposure is one
unauthenticated `OPTIONS` reaching a guest port, which returns headers and no data.

**Invariant — a credentialed cross-origin request must come from a sibling.** The
parent-domain cookie is what makes one login cover every app, and it is also what
lets *any* preview origin issue credentialed requests to *any other*. CORS alone does
not contain this: it blocks reading a response, not sending the request. So the edge
rejects a cross-origin request whose `Origin` is neither the target app itself nor
another app of the **same session**. Same-session siblings additionally get
`Access-Control-Allow-Origin`, `Access-Control-Allow-Credentials`, and `Vary: Origin`
only where the app emitted none — the edge never overwrites the app's own headers.

**`Host` keeps its rewrite; `Origin` is never touched.** The edge continues to send
`Host: localhost:<port>` upstream. Dev servers check `Host` and reject an unknown
value outright — Vite and rspack/rsbuild both do, and engrams' own
`web/vite.config.ts:146` already carries an `allowedHosts` list precisely because
tunnels trip that check. Rewriting keeps every such server working untouched, with no
per-app configuration.

The cost is that an app deriving absolute URLs from `Host` still emits `localhost`;
the peer-address variables above are the answer to that, and they are a better answer,
because they also configure services that never see the request at all.

`Origin`, however, is passed through verbatim, and `X-Forwarded-Host` /
`X-Forwarded-Proto` carry the real address. Codespaces rewrites `Origin` to
`http://localhost:8000`, which destroys an app's ability to build its own allowlist
and breaks framework CSRF checks; they declined to fix it. We do not repeat it.

**Invariant — preview hosts terminate.** Any request whose `Host` is under the
preview base domain is answered by the preview handler. It returns 404 when the host
names no live app. It must never call `next()`. Today the handler falls through
(`preview-proxy.ts:373`); IAP hides the consequence. With IAP off, a fall-through
would expose the whole orchestrator API without authentication. The same rule applies
to the WebSocket upgrade path.

**Invariant — the bridge skips preview hosts.** `iapBridge` runs before the router
and fails closed when `IAP_AUDIENCES` is set (`auth/iap-bridge.ts:552-566`). It gets a
**host-based** exemption for the preview base domain. A path-based exemption is the
wrong axis and would weaken the main host.

**Ordering.** The orchestrator wall must be merged and deployed **before** IAP is
turned off. The reverse order makes every preview public.

### App to app inside the guest

Same session only, in this version.

The guest resolves a sibling's hostname and connects on 443. The host's iptables
rule already redirects all guest traffic on 443 to the egress proxy (ADR 0006). The
proxy peeks the SNI, recognises a hostname belonging to **this** session, mints a
leaf with the CA the guest already trusts, terminates TLS, and splices the bytes
straight into the sibling's guest port through the ADR 0066 vsock relay.

```
guest app A ──https──▶ host egress proxy (SNI peek)
                         │ this SNI is my own session's app "api" → port 8080
                         ▼
                 open_guest_stream(sandbox, 1030) + RelayConnect{8080}
                         ▼
                  guest 127.0.0.1:8080  (app B)
```

One host hop. No DNS lookup off-box, no load balancer, no orchestrator, no
credential — the two apps are already in one trust domain.

Supporting decisions:

- **The filtering DNS proxy resolves a session's own app names**, checked *before*
  the deny-list and the allow-list. A sibling app is not an egress destination — the
  connection never leaves the host — so a session should not have to ask permission
  to look up its own address. No manifest change and no `allow_host_patterns` entry
  is needed.

  The name is then forwarded to the upstream resolver like any other allowed name,
  rather than answered with a synthetic address. The answer does not matter: the
  iptables redirect captures port 443 whatever the destination address is, so the
  SNI short circuit fires either way. Forwarding is simply the path that already
  exists, and it introduces no new failure mode — a preview base domain that
  browsers reach necessarily resolves publicly.
- **The short circuit is checked before the allow list**, next to the secret, inject,
  and observe arms that already take precedence.
- **The splice is raw**, not an HTTP proxy. WebSocket and h2c pass through unchanged.
  This means the callee sees the app's **real** hostname in `Host`, where the browser
  edge would have sent `localhost:<port>`. The divergence is deliberate — a raw splice
  cannot rewrite without parsing HTTP, and parsing would cost the protocol
  transparency the splice exists for — but it has one consequence worth stating: a
  callee that rejects an unknown `Host` refuses the call. In practice the services
  reached this way are APIs, which do not host-check, while the services that do are
  dev servers, which are callers rather than callees. If that stops being true, the
  fix is to add the app's hostname to that server's allowed-hosts list, not to make
  the splice parse HTTP.
- **`apps` is the LAST field of `SessionEgressPolicy`.** That type crosses the
  coord↔host wire as positional bincode (ADR 0013), so field order IS the format:
  a non-trailing insertion shifts every byte after it and desyncs a peer built
  from an older tree across a roll. A trailing field is the only wire-safe
  evolution, and `engram-protocol/tests/wire_golden.rs` pins the bytes to enforce
  it — the regenerated corpus must be a pure APPEND, with every pre-existing byte
  unchanged.
- **A new narrow seam.** `engram-egress-proxy` gets a `GuestPortDialer` trait that
  returns the `TunnelStream` it already defines. `engram-host-agent` implements it
  over `SandboxBackend::open_guest_stream`. `proxy_port::open_vsock_tunnel_at` is
  refactored to expose its connect half, so the `RelayConnect`/`RelayAck` handshake
  exists once, not twice.

### Rejected: one hostname per session, apps at path prefixes

Serving every app from one origin under a path prefix would delete this ADR's whole
CORS and cookie problem. It is a real design, and several products ship it (Vercel
Microfrontends, Netlify proxy rewrites, Tailscale `serve --set-path`). We do not take
it, for two reasons.

**Isolation.** A shared origin is a shared security boundary: one app's script can
reach another's DOM and reuse its cookies. Coder, which supports both, is explicit
that path-based apps *"share the same origin … which can expose the deployment to
cross-site-scripting attacks"* and that *"a malicious workspace could reuse cookies to
call the API or interact with other workspaces"* — their flags for it are literally
named `--dangerous-allow-path-app-sharing`. That argument is stronger here than
anywhere else, because the code inside a sandbox is written by an agent.

**Apps expect to own `/`.** Mounting a service under a prefix breaks its absolute
links, its asset paths, and its cookie `Path`. Coder cites the same problem for hot
reload and asset serving.

Per-app hostnames keep each app in its own origin, which is what the isolation
argument wants, and the peer-address variables above are what makes them workable.

### Rejected: send the call out and back

The guest could call the public URL, cross the internet, hit the load balancer, and
come back through the orchestrator and the coordinator into the same host. That path
needs a machine credential in the sandbox, an allow-list entry, and it adds a full
internet round trip to a call between two processes on one machine. Reliability and
latency are not negotiable here, so the short circuit wins.

Cross-session app-to-app is deferred. It needs a machine credential on the egress
inject plane and an authorisation model for which session may call which. The inject
plane (ADR 0056 Plane B) is the seam it would use.

## Consequences

- **Clean break.** `port_exposure` and `profile.port_exposures` are removed, not
  shimmed. Existing preview links stop working. The repo has no external users, and a
  compatibility shim here would mean two naming schemes and two auth paths forever.
- Ad-hoc exposure survives as an app with a derived name (`port-3000`), so the product
  keeps the capability and loses the second concept.
- Two long-standing gaps are closed in passing: `port_exposure` rows are never deleted
  when a session ends (no foreign key, no sweep), and `expires_at` is enforced but
  never written. Session teardown now deletes app rows, and the column is dropped.
- The preview data plane still runs through Bun. ADR 0064's deferred move to a Rust
  proxy is still deferred, and is unaffected by this ADR.
- ADR 0064 stayed `Proposed` although P1 to P4 all landed. This ADR supersedes it and
  carries the record forward.

## Phases

Each phase is one PR.

- **P1 — the model.** The `session_app` table, `profile.apps`, the batched pre-create
  reservation, env injection through `harness_env`, the `apps` proto fields, and the
  web editors. Retires `port_exposure` and `profile.port_exposures`.
- **P2 — the wall.** The parent-domain cookie, the host-based bridge exemption, the
  termination invariant, org visibility, the login redirect, the unauthenticated
  `OPTIONS` path, and sibling-origin enforcement.
- **P3 — app to app.** `apps` through `RuntimeSpec` and `SessionEgressPolicy` to the
  egress proxy's session state, the DNS answer, the SNI short circuit, and the
  `GuestPortDialer` seam. One small Firecracker integration test, wired into
  `ci.yml`'s `--test` list.
- **P4 — deploy (the private deploy repo).** Turn IAP off on the preview Gateway, drop the
  preview audience from `IAP_AUDIENCES`, set the cookie domain. Only after P2 is live.
- **P5 — validate against the deployed stack, then flip this ADR to Accepted.**
