# ADR 0083: egress-proxy bind failure is fatal (fail-closed startup)

**Status:** Accepted (2026-07-07). The per-host egress proxy (ADR 0006) is the **only**
path a guest reaches the network: iptables on the FC host installs a `:443 -> proxy` (and
`:53 -> dns`) REDIRECT at host-agent startup, and a `FORWARD` default-deny makes the proxy
the sole egress (ADR 0006, issue #240). This ADR makes a proxy **bind** failure fatal to
host-agent startup — the host aborts (fail-closed) instead of running on with a live
REDIRECT pointing at a dead port. Implemented on branch `fc-colima-dev`
(`crates/engram-egress-proxy/src/proxy.rs`, `crates/engram-host-agent/src/egress.rs`).

**Related:** ADR 0006 (the egress proxy + why it's mandatory), ADR 0014 (the FC per-VM
iptables REDIRECT the proxy sits behind), issue #240 (egress is mandatory; host-agent
refuses to start without it).

## Context

Observed live on the fc-colima dev rig: a `claude` session failed with
`API Error: Unable to connect to API (ConnectionRefused)` despite `api.anthropic.com`
being in the session's `allow_hosts`.

The iptables layer was correct — the failure was one level down:

1. The host-agent was running with a live REDIRECT `-A PREROUTING … --dport 443 -j
   REDIRECT --to-ports 8443` and the matching `INPUT … --dport 8443 -j ACCEPT`.
2. But **nothing was listening on 8443** (nor on the DNS port 5353) — the only listeners
   were the metrics (9100) and gRPC (9101) ports.
3. So the guest's TLS SYN to `api.anthropic.com:443` was REDIRECTed to `127.0.0.1:8443`,
   where no socket was listening; the kernel replied with a **RST**, and the guest's TLS
   client reported `ConnectionRefused`.

This is the signature that distinguishes it from a wrong allowlist: a `FORWARD DROP` (bad
`allow_hosts`) yields a **timeout**; a RST from a dead REDIRECT target yields
**`ConnectionRefused`**. The allowlist was never the problem.

**Why the proxy was dead.** Two coupled defects:

- `Proxy::run()` bound the 443 listener, then the DNS sockets, then looped on `accept`. A
  **DNS-socket bind failure `return`ed** from `run()`, tearing down the already-bound 443
  listener with it — one failed bind killed the whole proxy.
- `HostEgress::spawn` returned `Ok` the instant it `tokio::spawn`ed the run-loop task —
  **before any bind was attempted**. So a bind failure surfaced only as a log line from
  the dying detached task; `spawn` had already reported success, and `main.rs`'s
  "egress is mandatory, abort on failure" guard never fired. The host-agent kept serving,
  its gRPC readiness probe stayed green, and the coordinator kept scheduling sessions to a
  host whose every session was doomed to `ConnectionRefused`.

The bind failure itself was transient (a socket race when Tilt restarted the host-agent
over the previous instance's not-yet-released sockets — both ports were free moments
later). But the *architecture* turned a transient, self-healing race into a permanent,
silent, fleet-visible outage on that host. That is a fail-**open** hole in a path issue
#240 explicitly designed to be fail-closed.

## Decision

**Bind before serve, and treat a bind failure as fatal.**

- Split `Proxy::run()` into `Proxy::bind() -> Result<Listeners, io::Error>` (binds the
  443 listener **and** the DNS sockets synchronously) and `Proxy::serve(listeners)` (spawns
  the DNS serve loops and runs the `accept` loop). All proxy state stays on `Proxy`;
  `Listeners` is a small opaque handle carrying only the bound sockets, so the split adds
  no duplicated fields. A DNS bind failure is now returned as a value alongside a 443 bind
  failure — both are equally fatal, because the guest's `:53 -> dns` REDIRECT is live too,
  so a dead DNS socket breaks resolution the same way a dead 443 socket breaks HTTPS.
- `HostEgress::spawn` now `bind`s (synchronously, with a bounded retry) **before**
  spawning the accept-loop task, and returns `EgressError::Bind` on failure. `main.rs`
  already aborts host-agent startup on any `EgressError` — so the host now fails closed:
  it never runs with a live REDIRECT and no listener.

**Bounded retry, then abort.** `bind_with_retry` retries the bind a few times (5 attempts,
500 ms apart) to ride over the transient restart race *in-process* — no crashloop for the
common case. A bind that still fails is a genuine conflict (something permanently owns the
port), and there the abort is correct: under a supervisor (K8s per ADR 0044, or a Tilt
retrigger) the host crashloops **loudly** and drops out of the schedulable fleet, rather
than silently serving broken sessions.

**Tradeoff considered and rejected — fail-open (log-and-continue).** Keeping the host
alive with egress disabled has no upside: issue #240 already established there is no useful
session without egress, and a "healthy" host that breaks every session it's handed is
strictly worse than one absent from the fleet. Fail-closed + bounded retry dominates it:
it absorbs the transient race *and* surfaces the permanent conflict.

## Consequences

- A permanent port conflict now blocks host-agent startup (intended — the fleet routes
  around a down host; it cannot route around a lying one).
- `EgressError::Bind`, previously declared but never constructed, is now the live signal
  for this failure and carries the underlying `io::Error` for operator diagnosis.
- The DNS-proxy bind is now an explicit `HostEgress::spawn` parameter rather than a hidden
  `ProxyConfig` default. Making a bind failure fatal turned the previously-swallowed fixed
  `5353` bind into a real collision when multiple proxies stand up in one process (the test
  suite); the port is now the caller's decision — prod passes the port the iptables
  `:53 -> dns` REDIRECT targets, tests pass `None` to skip the DNS listener.
- Port `0` is rejected at startup for both `--egress-proxy-port` and
  `--egress-dns-port`: a `0` *bind* succeeds (ephemeral port) while the iptables
  REDIRECT still targets literal `0`, so the host would boot green with every
  guest getting `ConnectionRefused` — a split-brain the bind-failure guard alone
  cannot catch. There was already no `0 = off` sentinel (egress is mandatory);
  now it's enforced.
- No image re-bake, schema change, or guest change — host-agent binary only.
