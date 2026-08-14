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
one `(session, port)` pair, served at `<slug>.preview.engrams.cortex.io`. ADR 0066
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

The edge makes this worse. It rewrites the request to `Host: localhost:<port>`
(`orchestrator/src/routes/preview-proxy.ts:294`). An app that builds absolute URLs
from `Host` therefore emits `http://localhost:3000/...` links to the browser. That
rewrite was necessary before ADR 0066, when the host dialed the guest's external
interface. It is now the opposite of what we want.

**2. The wall is the wrong shape.** IAP fronts the preview Gateway
(`engrams-internal/modules/engrams/preview-gateway.tf:94-112`). It keeps the public
internet out, which is correct and must stay true. But IAP challenges each hostname
with a redirect to Google. A browser can follow that redirect for a top-level
navigation. A cross-origin `fetch()` from one app's page to a sibling app's origin
cannot: the browser sees a redirect to a foreign origin and CORS rejects it. A call
made from inside the guest carries no IAP assertion at all.

**3. Reservation is slow.** The auto-mint loop runs after `createSession` returns
and costs three serial round trips per port
(`orchestrator/src/db/port-exposures.ts:94-121`: SELECT, INSERT, SELECT).

## Decision

Retire the **exposed port**. Add the **session app**.

An app is a name, a guest port, and the environment variables that carry its public
address:

```
app { name, port, host_env?, url_env? }
```

The platform reserves one hostname per app **before it creates the session**, gives
the guest those addresses as environment variables, puts every app URL behind the
same login users already hit, and lets two apps in one session reach each other
without leaving the host machine.

### The hostname

`<app-name>-<session-slug>` — one DNS label under the preview base domain. The
session draws one random tri-word slug (`orchestrator/src/ports/slug.ts`); every app
in that session shares it. The label is readable, it is a single label so the
existing wildcard certificate covers it, and it still does not encode the port. The
whole label is the routing key, so the platform never parses it back into parts.

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

- Widen the session cookie to the parent domain (`.engrams.cortex.io`). One login
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
- The edge answers `OPTIONS` preflight and adds `Access-Control-Allow-Origin`,
  `Access-Control-Allow-Credentials`, and `Vary: Origin` when the `Origin` is another
  app **of the same session**. It never overwrites a header the guest app already
  set.
- The edge stops rewriting `Host`. Apps then emit correct absolute URLs. A dev server
  that rejects unknown hosts (Vite) needs `server.allowedHosts`; that is the correct
  trade now that ADR 0066 reaches loopback without the rewrite.

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

- **The filtering DNS proxy answers a session's own app names** with a synthetic
  address, checked *before* the `allow_hosts` filter. The redirect captures port 443
  whatever the destination address is, so the address does not matter. No manifest
  change and no `allow_host_patterns` entry is needed.
- **The short circuit is checked before the allow list**, next to the secret, inject,
  and observe arms that already take precedence.
- **The splice is raw**, not an HTTP proxy. WebSocket and h2c pass through unchanged,
  and the real `Host` header survives.
- **A new narrow seam.** `engram-egress-proxy` gets a `GuestPortDialer` trait that
  returns the `TunnelStream` it already defines. `engram-host-agent` implements it
  over `SandboxBackend::open_guest_stream`. `proxy_port::open_vsock_tunnel_at` is
  refactored to expose its connect half, so the `RelayConnect`/`RelayAck` handshake
  exists once, not twice.

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
  termination invariant, org visibility, the login redirect, sibling CORS, and the
  `Host`-rewrite reversal.
- **P3 — app to app.** `apps` through `RuntimeSpec` and `SessionEgressPolicy` to the
  egress proxy's session state, the DNS answer, the SNI short circuit, and the
  `GuestPortDialer` seam. One small Firecracker integration test, wired into
  `ci.yml`'s `--test` list.
- **P4 — deploy (engrams-internal).** Turn IAP off on the preview Gateway, drop the
  preview audience from `IAP_AUDIENCES`, set the cookie domain. Only after P2 is live.
- **P5 — validate against the deployed stack, then flip this ADR to Accepted.**
