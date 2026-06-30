# ADR 0064: Live-host guest ports at vanity subdomains

**Status:** Proposed (2026-06-29). engrams can run an agent that builds a web app inside a
session, but there is no way to *reach* a port that agent opened (a dev server on
`localhost:3000`). The only inbound path that exists today is the hardcoded ttyd shell
tunnel (ADR 0014 issue #6 / ADR 0039 §8): the egress proxy (ADR 0006) is strictly
outbound. This ADR adds a **generic raw-byte guest-port tunnel** — a protocol-agnostic
generalization of the ttyd `ProxyShell` path — and an orchestrator edge that maps an
auto-minted **vanity subdomain** (`jumping-fat-kittens.preview.engrams.cortex.io`) to a
`(session, port)` pair, behind IAP on the internal deployment and behind a
wildcard-localhost domain for local dev / VZ. Implemented across phases on dedicated
worktrees; this ADR is the bookend and is updated between phases.

**Related:** ADR 0014 issue #6 (the `ProxyShell` tunnel this generalizes), ADR 0013
(coord↔host gRPC transport), ADR 0039 §8 (the `ShellRelayService` app-tier bridge), ADR
0047 (PG is the routing authority — `resolve_sandbox`), ADR 0034 (idle eviction — the pin
this tunnel takes), ADR 0031 (user auth / owner-scoping / IAP ForwardAuth), ADR 0051 (the
orchestrator is the only web backend), ADR 0006 (egress proxy — the outbound counterpart),
ADR 0065 (live browser / computer use — a downstream consumer of this tunnel).

## Context

The ttyd shell already proves the whole inbound path works without new transport:

```
web (WS) → orchestrator/routes/shell.ts → coordinator grpc_app/shell_relay.rs
  (ensure_active auto-resume → resolve_sandbox → acquire_shell pin → proxy_shell)
  → host-agent proxy_shell.rs::open_shell_tunnel_at(guest_ip, 7681, netns)
  → guest ttyd:7681
```

The coordinator can reach hosts directly (gRPC dial-on-demand, ADR 0013), so inbound to a
guest port is just a coordinator→host bidi stream that dials a guest TCP port in the right
network namespace. The ttyd path is **WebSocket-frame-specific** (`ShellFrame`
text/binary/ping/pong/close, `ws://…:7681/ws`) and **port-pinned to 7681**. To host an
arbitrary dev server we need the same plumbing carrying **raw TCP bytes** to an
**arbitrary port**, plus an edge that authenticates and routes a public hostname to it.

(Aside fixed in passing: `proxy_shell.rs::connect_tcp_in_netns_linux` hardcoded
`TTYD_PORT` and ignored its caller's `port` argument — harmless today because prod always
dials 7681, but latent. Generalizing it to take a `port` both fixes that and lets the
port tunnel reuse the netns dial.)

## Decision

### Data path — a protocol-agnostic raw-byte port tunnel (P1, this phase)

A new tunnel that mirrors `ProxyShell` but carries opaque bytes, so HTTP/1.1, h2c,
WebSocket-upgrade, and gRPC all pass through transparently:

- **`engram-protocol`**
  - `host_service.proto`: `rpc ProxyPort(stream ProxyPortMessage) returns (stream
    ProxyPortMessage)`. First message MUST be `ProxyPortOpen{sandbox_id, port}`; subsequent
    messages are `ProxyPortData{bytes}` / `ProxyPortClose` in either direction. Same
    Open-is-a-distinct-oneof-arm discipline as `ProxyShell` (so the open sentinel can never
    be mistaken for data).
  - `session.proto`: `service PortRelayService { rpc Relay(stream RelayPortRequest) returns
    (stream RelayPortResponse) }` — the orchestrator→coordinator app-tier bridge, kept off
    the UI surface like `ShellRelayService`. `PortOpen{session_id, port}` first frame.
- **`engram-core`**: `types/port.rs` — `PortTunnel`/`PortTunnelEnds` (a pair of
  `mpsc::Sender/Receiver<Bytes>`; channel close = EOF, no frame enum). `HostClient` gains
  `async fn proxy_port(&self, sandbox_id, port) -> Result<PortTunnel>` with a default
  `Err(NotFound)` — only the host-agent local impl, the gRPC client, and the registry
  router override it (test fakes inherit the default, so the blast radius is tiny).
- **`engram-host-agent`**: `proxy_port.rs` exposing `open_tcp_tunnel_at(guest_ip, port,
  netns, ends)` — cold path raw `TcpStream::connect` (short connection-refused retry for the
  just-started-the-server race); warm path reuses `proxy_shell::connect_tcp_in_netns_linux`
  (now port-parameterized) to dial inside the per-VM netns. A raw byte pump
  (`pump_tcp_through_tunnel`) shuttles ≤64 KiB chunks each way. `LocalHostClient::proxy_port`
  resolves `vm_internal_ip` + `netns_name_for` (same as `proxy_shell`, minus the
  `start_shell` step — the dev server is user/agent-managed, not host-spawned) and opens the
  tunnel. The host `grpc_server.rs` serves `ProxyPort` (auto-registered via the
  `HostService` impl).
- **`engram-coordinator`**: `grpc_app/port_relay.rs` mirroring `shell_relay.rs` —
  `ensure_active` (auto-resume Idle) → `resolve_sandbox` (ADR 0047) → a pin against idle
  eviction → `proxy_port` → bidi byte bridge → RAII release on every exit path (incl. tonic
  cancel). P1 reuses the existing `acquire_shell`/`release_shell`/`renew_shell` hub
  refcount as the generic *interactive-attachment pin* (a shell and a port tunnel on the
  same sandbox both pin correctly); a later cleanup may rename it to a neutral `pin`.

### Edge / app — vanity subdomains + slug registry (P2-P3, later phases)

- **Slug registry** (orchestrator, Drizzle): `port_exposure { slug PK (tri-word, e.g.
  jumping-fat-kittens), sessionId, port, label, ownerUserId, visibility (private|shared),
  shareToken (nullable), createdAt, expiresAt }`. Slugs are minted server-side and random
  (no user-chosen names → no reservation/collision UX), and **do not encode the port** (no
  cross-session port-scanning).
- **Edge**: wildcard host `*.preview.engrams.cortex.io` → the orchestrator backend (same
  IAP-gated backend). A reverse-proxy handler keyed on the `Host` header parses the slug,
  looks up `(session, port, owner, visibility)`, authorizes, `ensure_active`s, opens the
  `PortRelayService.Relay` stream, and proxies the HTTP request / WS upgrade through it.
  Because IAP requires the L7 HTTPS LB (TLS terminates at the LB), the orchestrator does
  HTTP-level proxying with `Host`-header rewriting so guest apps that assume `localhost`
  still work.
- **Auth**: `principal == owner` OR `principal is admin (scope=all)` OR a valid `shareToken`
  for the slug. IAP stays on (caller must be an org member); the share link only widens
  *which* org member can view — it never bypasses IAP.
- **Triggers**: imperative (`POST /api/v1/sessions/:id/ports {port,label}` + a web "Expose
  port" action; `DELETE …/ports/:slug` revokes); agent-initiated via a harness control event
  up the existing channel (the guest never calls the orchestrator directly); optional
  declarative `profile.portExposures` (P4).
- **Local dev / VZ**: no IAP; synthetic-admin so the owner/admin check passes. A
  wildcard-localhost domain resolving to 127.0.0.1 (`*.lvh.me`) over plain http routes to
  the same handler — one code path, only domain/TLS/IAP differ by deployment.

## Phasing

- **P1 (landed, PR #478):** the Rust data path — `ProxyPort`/`PortRelay` protos, `PortTunnel`
  core types + `HostClient::proxy_port`, host-agent `proxy_port.rs` + server/client, the
  coordinator `port_relay.rs` service, and tests (unit byte round-trip + the `grpc_proxy_port`
  in-process wire test).
- **P2a (this change):** orchestrator port-exposure registry — the `port_exposure` table +
  migration, the tri-word slug generator, the data-access store, the CRUD route
  (`/api/v1/sessions/:id/ports`, owner/admin guarded), and the `portRelay` control-plane
  client. The edge proxy that serves the slugs lands in P2b, so the returned `url` is the
  eventual address, not yet reachable.
- **P2b (landed, PR #482):** the edge reverse-proxy data plane for **HTTP** — Host-based routing
  on `<slug>.<previewBaseDomain>` (a Hono middleware mounted first), auth (owner / admin /
  share-token), and the HTTP-over-`PortRelay` transport. The transport went through a design
  change: a custom `http.request({ createConnection })` Duplex does **not** work under Bun
  (Bun routes `http.request` through `fetch` and ignores `createConnection`, dialing the host
  for real → ECONNREFUSED). The shipped design stands up a one-shot **loopback `net.Server`**
  that raw-pipes a real local socket into a `PortRelay`-backed Duplex and `fetch()`es that
  local port — web types end to end, only Bun-supported APIs. Covered by a fake-relay e2e
  test that exercises the whole path on the Bun runtime.
- **P2b-ws:** WebSocket-upgrade passthrough (Vite HMR etc.). Split out because Bun's node:http
  `upgrade` handler can't write to the raw socket (see `orchestrator/src/server.ts` — the
  `socket.write`/`end` no-op bug), so raw WS passthrough needs its own approach + live
  validation. HTTP previews (page loads, assets, SSE, API) work without it.
- **P2c (this change):** web "Expose port" UI — a PORTS tab on the session detail page
  (`usePorts` React-Query hooks over the REST CRUD + a `PortsPanel`: expose a port, list
  exposures with their URL, copy the (share) link, revoke).
- **P3a (this change, OSS):** the generic deploy *mechanism* — the helm chart wires
  `ORCHESTRATOR_PREVIEW_BASE_DOMAIN` from a new `orchestrator.preview.baseDomain` value
  (emitted only when set, inert by default, mirroring the IAP/OIDC knobs). No ingress-template
  change is needed: the chart already ranges over values-supplied `web.ingress.hosts` with a
  per-path `service:` retarget (the Slack-webhook mechanism), so a `*.<baseDomain>` host →
  orchestrator is purely a values concern. The OSS/internal line: the chart hardcodes env
  keys (so the env wiring + a documented value belong here), while everything naming a
  specific domain/zone/cert/IAP-backend is company config → P3b.
- **P3b (engrams-internal):** the company-specific resources/values — set
  `orchestrator.preview.baseDomain: preview.engrams.cortex.io`, add the `*.preview.…` host to
  `web.ingress.hosts` (path `/` → orchestrator service, IAP-gated), the wildcard TLS cert
  (GCP Certificate Manager + DNS auth — google-managed certs don't do wildcards), and the
  wildcard DNS record. Drafted as a reviewable PR; a human drives the prod DNS/cert apply.
- **P3-validate:** live end-to-end against the deployed stack, then flip this ADR to Accepted.
- **P4 (this change, backend):** declarative `profile.portExposures` — a `repeated uint32`
  on the Profile message / `port_exposures` jsonb column (mirroring `skills`), settable via the
  ProfileService Connect API. At session-create the orchestrator auto-mints one **private**
  port-exposure per declared port (best-effort: a mint failure logs + continues, never fails
  the task), via the same `PortExposureStore.createOrGet` the imperative CRUD route uses. No
  per-port label/visibility in the declarative form (label = "", visibility = private). The web
  editor control for this field is a separate follow-up (backend-only here).

> Pitfall (P1 CI, fixed): the P1 proto change touched the TS-generated `session.proto` but
> the Rust-only P1 commit didn't regenerate/commit the checked-in TS bindings → the `buf`
> codegen-drift gate failed; and CI's clippy (floating `stable` = 1.96) flagged a
> `doc_lazy_continuation` (a doc line starting with `+ ` reads as a markdown bullet) that the
> stale local clippy (1.95) missed. Both fixed on the P1 branch. Lesson: run clippy via the
> pinned toolchain (`rustup update stable` / `nix develop`), and regenerate TS bindings
> (`just gen-proto`) in the same change as any `engram/app/**` proto edit.

## Consequences / open items

- **Latency**: P1-P2 route preview data through the orchestrator (Bun). If HMR/WS/large
  assets demand it, a later optimization mints a short-lived signed ticket and hands the
  data plane to a Rust proxy. Out of scope for v1.
- **The interactive-attachment pin** is shared with the shell (`acquire_shell`); a rename to
  a neutral `pin` is a candidate cleanup, deferred to avoid widening P1. The port relay
  **renews the pin every 60s** for the life of the connection (host-agent reaps pins
  un-renewed for `SHELL_PIN_STALE_AGE` = 300s), so a long-lived low-traffic preview isn't
  idle-evicted out from under the viewer.
- **Follow-up (noticed mid-P1, not papered over):** the gRPC `shell_relay.rs` bridge does
  **not** appear to call `renew_shell` periodically (no caller found in the coordinator
  request path), even though `harness.rs` documents "the coord shell bridge renews the pin
  every `SHELL_PIN_RENEW_INTERVAL`". Either renewal was dropped in the ADR-0039/0051 axum→gRPC
  shell migration (a latent bug: a >5-min idle-but-open shell could be reaped + idle-evicted)
  or there's a renewer this pass didn't find. Track and fix/confirm separately — the port
  relay does the renew correctly regardless.
- **Wildcard cert**: GCP google-managed certs historically don't do wildcards — P3 uses
  Certificate Manager with DNS authorization. To confirm at P3.
- **Message sizing**: the port pump chunks at 64 KiB so individual `ProxyPortData` messages
  stay well under tonic's 4 MiB default — no server message-size bump needed.
