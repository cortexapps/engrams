# ADR 0010: DNS filtering on the egress proxy

Status: accepted, 2026-05-13
Phase: 1 (landed) — `udp/53` + `tcp/53` proxy listeners and the
iptables REDIRECT in `crates/engram-sandbox-firecracker::net`.
Extends ADR 0006 (host-agent-owned egress proxy).

## Context

ADR 0006 placed the egress proxy on the host-agent and filtered
outbound TLS by SNI against `manifest.network.allow_hosts`. The
iptables shape that supported it carried two unconditional ACCEPTs:

```
-A FORWARD -s {pool} -p udp --dport 53 -d 1.1.1.1 -j ACCEPT
-A FORWARD -s {pool} -p tcp --dport 53 -d 1.1.1.1 -j ACCEPT
```

The reasoning was operational: DNS has to work for *anything* in
the VM to function (apt, ttyd, the harness's Anthropic API
client), and a public resolver is the smallest-surface dependency
to grant. Coupled with the explicit DROP for inter-VM, RFC1918,
link-local, loopback, and a default-deny on FORWARD, the
remaining VM-egress surface was tcp/443 (REDIRECTed to the proxy)
and udp/tcp/53 to 1.1.1.1.

The classic DNS-exfil channel that leaves wide open: a compromised
harness can encode arbitrary data into the **subdomain** part of
an attacker-controlled name. If the attacker controls the
authoritative resolver for `exfil.example.com`, then a query for
`base64-blob-of-secrets.exfil.example.com` reaches them as a
side-effect of the DNS lookup itself. The TCP/443 SNI filter sees
nothing — the query never reaches the connection layer. The
host's iptables sees what looks like a normal DNS query to 1.1.1.1.

The threat model that motivated ADR 0006's TLS-MITM proxy
considered post-compromise lateral movement and broker-mode
secrets, but not this. Closing it requires bringing DNS under the
same allow-list enforcement as connections.

## Decision

**Route all guest DNS through a filtering proxy that resolves only
names already in `manifest.network.allow_hosts`.** Concretely:

### Wire layer (`engram-egress-proxy::dns`)

The host-side egress proxy gains two new listeners, alongside the
existing `tcp/443` MITM listener:

- **`udp/5353`** — bound on the host's loopback. iptables-REDIRECTs
  guest `udp/53` queries here regardless of which upstream IP the
  guest's `/etc/resolv.conf` points at. Each query is parsed via
  `hickory-proto`, the first QNAME is extracted, and the source IP
  is looked up in the same `Registry::lookup` the TCP/443 path
  uses. If `network_allow.matches(qname)` (or any of the per-
  secret `allow_hosts` matches), the query is forwarded verbatim
  to `1.1.1.1:53` via a shared bound socket demuxed by message ID;
  the response is shipped back over the same socket. Otherwise the
  proxy synthesizes an `NXDOMAIN` response (preserving the query's
  EDNS OPT record if any) and sends it back.

- **`tcp/5353`** — same logic, with the 2-byte length prefix
  DNS-over-TCP adds. Without this listener, a guest could defeat
  the UDP filter by setting a small EDNS UDP-size, forcing TC=1
  (truncated) on the response, and retrying over TCP/53 — where
  the legacy `ACCEPT VM→1.1.1.1 tcp/53` rule was leaving a hole.

Port 5353 (not 53) so the host's own `systemd-resolved` (typically
bound on `127.0.0.53:53`) can keep running for the host's name
resolution. iptables REDIRECT rewrites the destination port; the
client sees what looks like a normal DNS exchange.

### iptables (`engram-sandbox-firecracker::net::host_startup_lines`)

The mandatory egress configuration drops the two unconditional
`ACCEPT VM→1.1.1.1:53` rules. Instead:

```
-t nat -A PREROUTING -i tap-engr-+ -p udp --dport 53 \
   -j REDIRECT --to-port 5353
-t nat -A PREROUTING -i tap-engr-+ -p tcp --dport 53 \
   -j REDIRECT --to-port 5353
-A INPUT -s {pool} -p udp --dport 5353 -j ACCEPT
-A INPUT -s {pool} -p tcp --dport 5353 -j ACCEPT
```

The `INPUT` accepts come *before* the blanket `engram-host-input`
DROP so REDIRECTed packets reach the proxy's listening sockets.

ADR 0083 and issue #240 removed the bring-up-only unfiltered mode. Port `0` is
rejected at host-agent startup because an ephemeral listener cannot match a
literal iptables REDIRECT target.

### Policy source

The same `HostList` the TCP/443 SNI peeker uses
(`engram-egress-proxy::policy`). Resolution and connection share
one allow-list. The manifest's `[network].allow_hosts` block is
the single operator-facing knob; whatever you put there decides
both "what TCP connection can be made" and "what DNS name can be
resolved." Wildcards (`*.example.com`) work for DNS the same way
they work for SNI.

### Registry plumbing

The host's egress `Registry::register` is populated by
`PooledBackend::notify_session_policy`, called from the WS
handler when the coord pushes `NotifyKind::SessionEgressPolicy`
after each session creation. The DNS path looks up by guest IP
(the source address of the UDP packet, recovered before iptables
DNAT rewrites the destination); the TCP path looks up by
`peer_addr` after `SO_ORIGINAL_DST` recovery. Both end up keyed
on the same `Ipv4Addr`.

The /30 allocator deterministically assigns each guest's IP
(network + `.2`), and `FirecrackerBackend::guest_ip` returns it
from `NetSetup` without a vsock round-trip to `engram-agentd`.
This matters because the coord's `notify_session_policy` call is
best-effort and racing the agent's boot — a 2-second vsock
timeout on `guest_ip` lookup loses the only opportunity to
register the policy.

## Consequences

- **Resolution is allow-listed.** A guest can only resolve names
  the operator already permitted a TCP connection to. The exfil
  channel ADR 0006 left open is closed.

- **The DNS path and the TCP path share a single source of
  truth.** Future allow-list editions (wildcards, per-secret
  policies, manifest-validation lints) automatically apply to
  both transports.

- **One operator-tunable port** (`5353` by default) — the choice
  was forced by the host's `systemd-resolved`; binding `0.0.0.0:53`
  conflicts with the loopback-bound resolver. Operators on hosts
  without a local resolver can rebind via a future
  `--egress-dns-port` flag; today the constant is in
  `engram-sandbox-firecracker::net::DEFAULT_DNS_PORT`.

- **DNS-over-HTTPS is the residual channel that can't be closed
  at the transport layer.** DoH endpoints (`cloudflare-dns.com`,
  `dns.google`, `doh.opendns.com`, `dns.nextdns.io`, …) look
  identical to any other HTTPS request to the same hostname.
  Mitigation is operator discipline: don't put DoH endpoints in
  `allow_hosts`. A future manifest-validation lint that warns when
  `allow_hosts` intersects a curated DoH list could automate the
  check, but new DoH endpoints appear and any cooperating
  webserver can host one, so hard enforcement isn't feasible.
  DNS-over-TLS (`tcp/853`) is already dropped by the
  default-deny on FORWARD when proxy mode is on; custom-port
  resolvers (`udp/12345`, etc.) likewise.

- **No DNSSEC validation on synthetic NXDOMAIN.** A real DNS
  server proving "no such name" returns NSEC/NSEC3 records the
  validator can chain. The proxy's NXDOMAIN is unauthenticated.
  glibc and Claude CLI don't validate; `systemd-resolved` with
  `DNSSEC=yes` and `unbound` would see SERVFAIL instead of clean
  negative. None of the current images use those, so this is a
  documented gap rather than a regression.

- **No TCP/53 hole.** Both transports filter through the same
  proxy. Truncated UDP responses correctly retry over TCP/53
  through the filter; queries denied via UDP stay denied via TCP.

## Alternatives considered

- **Drop the `ACCEPT VM→1.1.1.1:53` rules without a filtering
  proxy.** Rejected: DNS in the guest stops working entirely.
  Apt updates, the harness's API client, ttyd's hostname
  reverse-lookup all break. No path back without a proxy.

- **Block exfil at the resolver layer (e.g. require the guest to
  use `systemd-resolved` configured to only forward listed
  names).** Rejected: would require modifying every bake image's
  init shim, would tie the policy to in-VM config we can't
  trust on a compromised guest, and would still leave the
  iptables-bypass path (just dial 1.1.1.1 directly).

- **Hand-roll the DNS parser instead of pulling in `hickory-proto`.**
  Considered. Trade-off was robustness on malformed/edge-case
  queries (compression in queries, label-character edge cases,
  weird EDNS OPT records) vs ~30 transitive dependencies. Went
  with `hickory-proto` — the dependency cost is fixed; correctness
  bugs aren't. Hand-roll path is documented in the commit message
  for posterity if we ever need to revisit.

## Implementation

Landed in two commits:

1. `feat(egress-proxy): filtering DNS proxy on udp/53 + tcp/53`
   (`e27c331`) — the proxy module, the iptables changes,
   `WIRE_VERSION` housekeeping, 8 unit + integration tests.
2. `fix(egress): make split-mode FC actually route DNS through
   the proxy` (`120b16e`) — the six wiring gaps in `--mode=host`
   that left the filter dark: `fc.host_startup()` not called,
   `RemoteHostClient::guest_ip` returning None, FC's guest_ip
   dialing agentd instead of using the static /30, `SessionEgressPolicy`
   not dispatched to `backend.notify_session_policy` from
   `handle_notify`, `cli.egress_proxy_port` not plumbed into the
   FC config, stale TAPs from earlier sessions interfering with
   routing.
