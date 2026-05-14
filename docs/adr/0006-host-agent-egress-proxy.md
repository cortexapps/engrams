# ADR 0006: Egress proxy lives on the host-agent

Status: accepted, 2026-05-10
Phase: 7 (production deploy)

Refines the proxy ownership / CA distribution story implicit in
ADR 0001's egress-policy machinery. No prior ADR explicitly placed
the proxy; until this one, the coordinator ran it because that's
where the egress-policy code first landed.

## Context

Production sessions need egress filtering. Two reasons that together
forced the architectural decision:

1. **Hostname allow-listing.** Every image manifest's `[network]`
   block lists which hostnames the sandbox is permitted to reach.
   Enforcement has to happen at TLS-SNI time (we can't decide before
   the client sends ClientHello, and we can't decide after TLS is
   established because the cleartext is gone). The proxy SNI-peeks
   and rejects unmatched hosts.

2. **Broker-mode secrets** (`secret_mode = "broker"` in
   `engram.toml`). The guest's env carries placeholders
   (`engram_ph_<session>_<hash>`) instead of real values; the proxy
   MITMs the guest's outbound TLS, substitutes the real value for
   the placeholder in the cleartext request, and re-encrypts to the
   destination. The placeholder is what an exfil attempt would see;
   the real value never lives in the sandbox.

Both behaviors require MITM, which requires:

- A trusted CA in the guest's substrate (`.engram-host/ca.pem`) so
  the leaf the proxy mints validates.
- An iptables PREROUTING REDIRECT from the guest's tap-network
  tcp/443 to the proxy's listener.
- Per-session state at the proxy: source IP → (network policy, per-
  secret keyring).

The iptables REDIRECT is the load-bearing constraint. **iptables
rules apply locally on the FC host's tap network**. A REDIRECT to
`127.0.0.1:<port>` works only if a proxy is listening on that host's
loopback. Cross-machine alternatives all hurt:

- **DNAT to the coordinator's IP**: puts the coordinator in the per-
  request egress path, multiplying its blast radius and adding
  cross-machine latency to every TLS handshake.
- **Dedicated proxy service**: same DNAT problem plus a new
  deployment to operate.
- **Replicate iptables decisions in user-space at the coordinator**:
  loses MITM and the cleartext that broker mode requires.

## Decision

**The egress proxy runs on each FC host-agent.** Specifically:

- Each `engram-host-agent` instance spawns a `HostEgress` at boot
  (when `--egress-proxy-port > 0`). The proxy binds the host's
  loopback; iptables REDIRECT is local; no cross-machine traffic
  enters the request path.

- All host-agents in a deployment load the **same CA cert + key**
  via a pluggable `CaSource` trait
  (`crates/engram-egress-proxy/src/ca.rs`). v1 ships three impls:

  - `LocalDiskCaSource` — dev / single-host. Auto-generates on
    first boot.
  - `EnvCaSource` — reads PEM from two env vars. The "secrets are
    projected into my process env somehow" path.
  - `GcpSecretManagerCaSource` (in `engram-secrets-gcp`) — fetches
    both PEMs from named Secret Manager paths via Workload
    Identity. v1 production default.

  Adding AWS / Azure / Vault is one impl per backend.

- The host-agent stamps its CA cert into every harness substrate it
  builds (`engram-host-agent::image_cache::ensure_harness_ext4`),
  including the CA fingerprint in the cache filename so rotation
  invalidates cleanly.

- The coordinator ships per-session policy
  (`NotifyKind::SessionEgressPolicy` —
  `crates/engram-protocol/src/wire.rs`) to the host-agent that owns
  the sandbox via the existing WebSocket, immediately after
  `SandboxBackend::create_for_session` returns the sandbox_id and
  `guest_ip` is known. The host-agent registers against its local
  proxy registry; **WS frame ordering** guarantees the policy lands
  before the subsequent `start_agent` request on the same
  connection, so the harness can't make egress calls before the
  proxy knows about the session.

- Session unregistration is **local to the host-agent**, driven by
  `SandboxBackend::destroy`. The coordinator does no
  proxy-registry cleanup.

## Consequences

- **The coordinator stays out of the per-request egress path.** Its
  failure modes don't multiply with sandbox traffic; an FC host can
  serve egress through a proxy outage at the coordinator.

- **Adding a new cloud means writing one `CaSource` impl.**
  `engram-egress-proxy` stays cloud-agnostic; the impl crate
  decides its auth path.

- **The host-agent is the single security boundary per host.** It
  owns both the isolation (the sandbox lifecycle) and the egress
  policy. There's no separate trust hop between "what process owns
  the VM" and "what process gates its egress."

- **Broker mode works end-to-end.** The substitution logic in
  `engram-egress-proxy::substitute::substitute` was complete in
  ADR 0001's era but unreachable because the registration step
  never ran in multi-host topology. This ADR closes that gap.

- **A new coordinator→host out-of-band notify primitive** lands on
  the trait: `SandboxBackend::notify_session_policy`. First use
  beyond `HeartbeatAck`. Future cross-machine state-sync work
  (idle-evictor on host-agent, known-issues #7) can reuse the same
  pattern instead of inventing one.

- **`--mode=all` keeps working** because the host-agent module
  runs in-process; the dev `LocalDiskCaSource` fallback keeps
  `just dev` zero-config. The same `HostEgress` codepath runs in
  both single-binary dev and multi-host production.

- **Substrate CA injection is part of substrate-build**, not a
  separate step. `ensure_harness_ext4` takes the host's CA PEM and
  writes `.engram-host/ca.pem` before `mke2fs`. No image-build-
  time wiring needed — the substrate is built per-host anyway.

## Follow-up: DNS filtering (ADR 0010)

ADR 0006 scoped enforcement to outbound TLS (the SNI peek + the
MITM substitution path). DNS itself was an unconditional ACCEPT
to a public resolver — a known DNS-exfil channel that this ADR
deferred. [ADR 0010](./0010-dns-filtering.md) closes it by giving
the egress proxy two more listeners (`udp/5353` + `tcp/5353`) and
swapping the `ACCEPT VM→1.1.1.1:53` iptables rules for REDIRECTs.
Same allow-list, same `Registry::lookup`, same operator-facing
manifest knob (`network.allow_hosts`).

## Alternatives considered

- **Coordinator-side proxy with DNAT.** Rejected: latency, blast
  radius, and coordinator stays in the egress request path.
- **Separate proxy service.** Rejected: operationally heavier than
  the host-agent topology, no architectural win.
- **Skip MITM, enforce at the SNI/iptables layer only.** Rejected:
  can't do per-secret broker substitution; insufficient for the
  threat model that motivated broker mode.

## Implementation

Landed in nine commits:

1. `feat(egress-proxy): introduce CaSource trait + Env/LocalDisk impls`
2. `feat(secrets-gcp): add GcpSecretManagerCaSource`
3. `feat(protocol): add NotifyKind::SessionEgressPolicy + WireSecretEntry`
4. `feat(sandbox): add SandboxBackend::notify_session_policy`
5. `feat(host-agent): own egress proxy locally via HostEgress`
6. `feat(host-agent): inject host CA into harness substrates`
7. `feat(coordinator): route egress policy to host-agent via notify` (cutover)
8. `refactor(coordinator): delete v1 egress-proxy machinery`
9. `docs(adr): add ADR 0006 for host-agent egress topology` (this commit)
