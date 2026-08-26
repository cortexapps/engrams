# ADR 0121: Node-local egress proxyd — in-flight guest egress survives host-agent rolls

Status: Proposed (2026-08-26)

Terms used in this document:

- **Egress proxy**: our program on each node that filters and brokers a
  guest's outbound traffic (ADR 0006). Today it runs inside the
  host-agent process.
- **Roll**: the operator's image update of the host-agent DaemonSet
  (ADR 0044 K3): cordon, delete the pod, wait for the successor,
  uncordon. Rolls reattach; they do not evacuate sessions.
- **proxyd**: `engram-egress-proxyd`, the new node-local daemon this
  ADR introduces. It owns the proxy listeners and outlives host-agent
  pods.
- **Adopt**: the successor host-agent finds a live proxyd from a prior
  pod generation and continues to use it instead of starting a new one.
- **Policy**: one session's `SessionEgressPolicy` — the resolved,
  ready-to-apply form the coordinator sends the host (ADR 0111).

## Summary, in plain English

Every deploy that rebuilds the host-agent image rolls its DaemonSet.
The Firecracker VMs survive the roll by design: the FC and uffd-handler
processes escape the pod cgroup, and the successor pod re-adopts them
by pidfd (ADR 0044 K2). The NBD kernel bindings park guest I/O until
the successor reconfigures them. The applied egress policies survive on
disk (ADR 0111). The iptables rules survive in the node root netns.

The egress proxy's sockets are the one data-plane resource that still
dies with the pod. When the pod exits, the kernel resets every
established guest connection. An agent that holds a model-API SSE
stream mid-turn sees `API Error: Connection lost`; the turn fails; the
session parks. On 2026-08-26 one routine deploy (PR #1401) cut two live
sessions this way. The same class occurred on 2026-08-14. Agents hold
long turns, so every deploy is a stray-bullet risk to whatever is
mid-stream.

The fix is structural, the ADR 0044 K2 move applied to egress: take the
proxy out of the pod's kill domain. Host-agent spawns (or adopts) one
node-local daemon that owns the three proxy listeners. A roll then
replaces the control plane and leaves the data plane running. Streams
survive because the process that holds them survives. There is no
drain, no gate, no watchdog, and nothing to detect: the failure mode is
removed, not managed.

ADR 0076 proposed this lifecycle split for the NBD/UFFD/chunk-cache
data plane and gated it on an fd-retention design, because a crash of
that daemon means silent memory corruption for every guest. That gate
does not bind here: a proxyd crash closes TCP sockets — a loud
connection reset, the same failure every roll produces today. The
egress split takes the cheap end of the ADR 0076 shape without waiting
for the hard end.

## Decision

**Rule: guest egress connections terminate in a process no pod
lifecycle owns.**

1. A new bin crate, `crates/engram-egress-proxyd`, owns the three
   listeners (`:8443` proxy, `:5353` DNS udp+tcp, `:13338` guest
   gateway — one `Proxy::bind`, they move together) plus the registry,
   cert mint, tunnel pool, and the coordinator-facing callbacks the
   request path needs. A new small crate, `crates/engram-egress-proto`,
   carries the control protocol, the manifest schema, and the pure
   adopt decision.
2. Host-agent startup runs `ensure_proxyd()`: read the manifest at
   `<work_dir>/egress-proxyd.json`, probe the live process, then adopt
   it or replace it (see "Lifecycle"). Spawn is without `kill_on_drop`;
   proxyd migrates itself into
   `<ENGRAM_FC_VM_CGROUP_PARENT>/egress-proxyd` before it binds, so
   GKE's pod-cgroup kill cannot reach it (the ADR 0044 K2 escape).
3. Policy application becomes a control-plane RPC over a UDS at
   `<work_dir>/egress-proxyd.sock`. Host-agent stays the sole owner of
   the ADR 0111 `bindings/*.egress` files and their persist-before-ack
   contract. On every control-socket connect, host-agent sends the full
   surviving policy set (`SyncPolicies`); proxyd replaces its registry
   from it and drops entries not in the set. One idempotent flow covers
   live registration, roll recovery, proxyd crash recovery, and
   stale-entry pruning. No second reader of the files, no watcher.
4. The wire-to-proxy policy translation (`register_policy`,
   `graphql_match`) moves into proxyd. Policies travel over the UDS as
   the wire `SessionEgressPolicy` — the type ADR 0111 already persists.
5. The coordinator-facing callbacks the request path depends on — the
   inject refresher (WS4), the observe sink (ADR 0056 Phase 4), and the
   Cloud SQL endpoint factory — move into proxyd, with a slim HTTP
   client that carries exactly those three routes. If they stayed in
   host-agent, a guest request that needs a credential re-mint during a
   roll gap would fail — the failure class this ADR deletes. Coord URL,
   token, and CA material arrive as spawn-time env/args (the
   uffd-handler pattern); rotation is a config mismatch at adopt time
   and triggers a replace.
6. The ADR 0118 app relay dials back: proxyd asks host-agent over a
   second UDS (`<work_dir>/egress-dialback.sock`) to open the guest
   stream. Host-agent opens it (backend + ADR 0066 relay handshake
   stay there), passes one end of a socketpair to proxyd via
   SCM_RIGHTS, and pumps the other end against the guest stream.
   *Implementation divergence from the first draft*: the raw guest
   fd is NOT passed — FC's guest streams are epoch-severed wrappers
   (capture semantics) and VZ's are in-process objects, so a raw-fd
   pass would bypass both. The pump means an established app-relay
   stream still traverses the pod and dies with it — deliberately
   accepted: ADR 0066 already treats app-relay resets on lifecycle
   events as "the browser reconnects", and the streams this ADR
   exists for (model API, DNS, tunnels) never touch the pump. A NEW
   app-relay dial during the roll gap fails and the client retries.
7. The shutdown ladder gets **no** egress rung. SIGTERM must ignore
   proxyd: the daemon is not the pod's to stop. This is deliberate — do
   not "fix" it.

## Lifecycle

### Spawn

proxyd ships in the host-agent image next to `engram-uffd-handler`.
Host-agent spawns it from the pod's own image filesystem. The running
process survives pod teardown on mapped text, exactly as FC survives
the deletion of its per-pod emptyDir binary today. A new exec only ever
happens from a live pod's image, so no binary is staged to the host.

Startup order inside proxyd: migrate into the node cgroup → bind the
control UDS → bind the three listeners (`bind_with_retry`, 5×500 ms,
moved from host-agent) → write the manifest (atomic temp+rename) →
serve. A bind failure exits non-zero with no manifest written.

The manifest records what the successor needs to decide: pid,
start-time jiffies, comm (the three-axis identity, the
`sandbox_manifest` precedent), the source fingerprint, the three ports,
and the control-socket path.

### Adopt

`decide_adopt(manifest, proc_identity, expected_fingerprint,
expected_config) -> AdoptPlan` is a pure function with a truth table
test (the `spawn()`/`run_once()` idiom). Outcomes: `Adopt`,
`RestartForUpgrade` (graceful `Shutdown` op, then spawn),
`KillStaleAndSpawn` (identity mismatch — a recycled pid), `SpawnFresh`.
Port or CA or coord-URL mismatches are upgrades; identity failures are
stale kills.

Adoption additionally requires two live checks, both one-shot at adopt
time (not a watchdog):

1. `Hello`/`HelloAck` over the control UDS: protocol version, source
   fingerprint, bound ports, CA fingerprint, coord URL.
2. An accept-loop probe: a TCP connect to `127.0.0.1:8443` must be
   accepted and then promptly closed (the registry's `NoSession` drop).
   The close proves the dispatch path ran; a kernel backlog handshake
   alone proves nothing. A wedged proxyd fails the probe and is
   replaced.

### Fail-closed (the ADR 0083 invariant, translated)

Host-agent aborts startup unless `ensure_proxyd()` confirms a serving
proxyd within its budget — the same abort `build_host_egress` performs
today on a bind failure. The invariant is unchanged: the iptables
REDIRECT never targets a dead port while the host reports healthy;
"bound in-process" becomes "confirmed serving over the UDS". Port 0
stays rejected.

### Supervision (event-driven, no scanner)

Host-agent holds the spawned child handle (`child.wait().await`) or the
adopted pidfd (`AsyncFd` readability — the documented exit-detection
pattern in `pidfd.rs`). Process exit is an event, not a poll. On exit:
respawn, reconnect, `SyncPolicies`, with bounded backoff. Persistent
respawn failure logs at ERROR and keeps retrying on each exit event; a
steady-state host-agent never aborts for it (that would take down the
control plane for live VMs). While proxyd is down, guests fail closed
(RST) — today's roll-gap behavior, now bounded by respawn instead of by
pod rollout. If host-agent and proxyd are both down, the successor
pod's `ensure_proxyd` covers it.

## Upgrades: the source fingerprint

A running proxyd must be replaced when its code changes. Replacement
cuts that node's streams (established MITM state — paired rustls
sessions — cannot transfer between processes), so the trigger must fire
exactly when the source actually changed:

- At image build, a script computes a content hash over the source of
  proxyd's cargo dependency closure (the `detect-rebake-lanes.py`
  closure logic) and injects it into **both** binaries via env +
  `option_env!`.
- The successor compares its embedded value against the manifest and
  `HelloAck`. Mismatch → `RestartForUpgrade`.
- **Never compare binary hashes.** Builds are not reproducible; a
  binary-hash compare would restart proxyd on ~every deploy and defeat
  the design.
- A local/dev build without the env treats fingerprint as unknown and
  always replaces (dev spawns fresh; no adoption confusion under Tilt).

SO_REUSEPORT generation handover (new proxyd binds alongside, old one
drains) is deliberately **not** built: it cannot save established MITM
streams, which are the streams that matter. The `Shutdown` op and the
manifest schema version leave room if that ever changes.

## What this defends against

Pod death: deploys, host-agent panics, OOM kills of the pod. Every
established guest stream — model-API SSE, Cloud SQL tunnels, app-relay
splices — keeps flowing, and new connections keep working, because
listeners, registry, and coord callbacks all live in proxyd.

## What this does not defend against

- **proxyd source changes.** A deploy whose image carries a new
  fingerprint replaces proxyd and cuts that node's streams once. The
  frequency drops from every-deploy to proxy-source-deploys.
- **proxyd crash.** All node streams cut; fail-closed refusal until the
  event-driven respawn. Same severity as one roll today.
- **The introducing deploy.** The first roll to a proxyd image cuts
  streams once (the old pod's proxy was in-process).
- **App-relay streams during a host-agent gap.** New dials fail until
  the successor connects, and established relayed streams die with the
  pod (they traverse the dial-back pump — see Decision §6). Every
  other established stream and the in-proxyd refresh path keep
  working.
- **Turn-level recoverability.** A cut SSE stream is still not
  resumable at the app layer. This ADR removes the routine cause; it
  does not make severance survivable.
- **Node death.** Unchanged; the ADR 0028 ladder owns it.

## Security posture

proxyd runs as root on the node, in the same trust domain as the
host-agent that spawns it, with the same inputs the pod already holds
in env (CA material, coordinator token). The control UDS and dial-back
UDS live in `<work_dir>` (root-owned, 0700-class hostPath) — the same
exposure as `substrate.sock` and the ADR 0111 policy files. No new
secret class touches disk; policies transit a root-owned socket instead
of process memory.

## Rejected alternatives

- **Listener-fd handoff (SCM_RIGHTS escrow / pidfd_getfd).** Moves
  listener fds fine, but the connections that matter are MITM'd: two
  paired rustls sessions with keys, sequence state, and in-flight
  plaintext per stream. None of it is serializable. Handoff saves only
  the bind gap — strictly weaker than proxyd at similar plumbing cost.
- **In-guest retry layer.** Watchdog-shaped, cannot resume a
  half-consumed SSE response (no `Last-Event-ID` on the Messages API),
  and depends on third-party CLI retry behavior we do not control.
- **Roll gate on active streams.** Contradicts the ADR 0044 K3
  amendment ("image rolls reattach, they do not evacuate"); agent turns
  run up to 24 h, so the gate is always full; and it is a poll.
- **preStop linger.** hostNetwork + fixed ports means the successor
  cannot bind until the old pod fully dies; linger postpones the cut by
  at most `terminationGracePeriodSeconds` while blocking the deploy.
- **A second DaemonSet for the proxy.** Its own roll still cuts
  streams, un-schedulably; the fail-closed invariant becomes a
  cross-pod readiness coupling; and it adds an image, a bake lane, and
  an operator surface. The data-plane plumbing (UDS, policy sync,
  dial-back) is identical in both designs, so it is not simpler either.

## Testing

- Pure unit (`engram-egress-proto`): `decide_adopt` truth table (fresh
  node, healthy adopt, fingerprint mismatch, dead pid, recycled pid via
  start-time, port change, corrupt manifest); manifest round-trip;
  frame round-trip.
- Plain-Linux bin test (no KVM): spawn proxyd on ephemeral ports in a
  temp work dir, sync a loopback policy, TLS round-trip through the
  proxy, kill -9 → respawn + sync, and adopt across a client restart
  with a pre-kill stream still flowing.
- Firecracker (CI KVM lane): `egress_stream_survives_roll` — one real
  VM holds one TLS stream through proxyd from a hermetic trickle
  fixture across a generation-A→B host-agent replacement; bytes flow on
  the pre-roll connection after the roll, and a new connection works.
  Sized to the property: one guest, one stream. The existing
  `egress_survives_roll` (policy replay) is retargeted at the UDS path.

## Rollout

Ships as a stack: this ADR; the two crates (built, unwired); the
host-agent switchover; the FC stream-survival test + CI wiring. The
chart adds `ENGRAM_EGRESS_PROXYD_BIN`; no new mounts (work dir and
cgroupfs are already mounted). The introducing deploy cuts streams once.

**Rollback hazard**: rolling back to a pre-proxyd image leaves an
orphan proxyd holding the three ports; the old in-process bind then
fails and the pod crashloops — correctly fail-closed, but stuck.
Runbook: `pkill -f engram-egress-proxyd` on each node (or kill via its
cgroup) when rolling back across this boundary.

## Relationship to other ADRs

- **ADR 0006** (egress proxy): filtering, MITM, and registry semantics
  unchanged; only the owning process changes. Its rejection of a
  *cross-machine* proxy service stands; this daemon is node-local.
- **ADR 0044 K2** (VM detach/reattach): the pattern this document
  applies to egress — escape the pod cgroup, record identity durably,
  re-adopt by pidfd.
- **ADR 0111** (honest egress registration): unchanged and load-
  bearing. The persisted policies are what `SyncPolicies` replays; the
  persist-before-ack contract keeps host-agent the sole file owner.
- **ADR 0083** (bind is fatal): the fail-closed invariant survives as
  "confirmed serving over the UDS before ready"; `bind_with_retry`
  moves into proxyd.
- **ADR 0076** (engram-substrated): the same lifecycle split, for the
  subsystem where a daemon crash is loud instead of corrupting. This
  ADR neither implements nor forecloses 0076.
- **ADR 0118** (session apps): the app relay keeps working through the
  dial-back seam; only its dial path moves.
