# ADR 0066: vsock port relay — reach the guest's `127.0.0.1` for live previews

**Status:** Accepted (2026-07-01) — shipped in three PRs merged to `main`: P1 (FC + Process
relay) #512 (`80d914ab`), P2 (VZ real vsock) #514 (`0a3fe3d4`), P3 (shell unification) #513
(`0b9ca1da`). ADR 0064 gave us a raw-byte guest-port tunnel, but the
host-agent reaches the guest by dialing **`guest_ip:PORT` over the guest's network
interface**. That reaches only servers bound to `0.0.0.0`/`::`; a dev server bound to
**`127.0.0.1`** — the default for Vite, the Tilt UI, `next dev`, CRA, Rails, Flask — is
invisible, so its preview never loads. This ADR moves the guest hop onto a **vsock relay**:
the in-guest `engram-agentd` dials `127.0.0.1:PORT` itself and splices bytes, which is how
every port-forward tool with an in-guest agent works (`kubectl port-forward`, `docker -p`,
Codespaces). Implemented across phases on dedicated worktrees; this ADR is the bookend and
is updated between phases.

**Related:** ADR 0064 (the port tunnel this fixes — specifically its "produce a guest
stream" step), ADR 0023 (the forge vsock bridge — the dedicated-port framing template),
ADR 0006 (egress proxy — the guest-networking layer the rejected DNAT alternative would
collide with), ADR 0013 (coord↔host gRPC transport, untouched), ADR 0044 (the per-VM netns
whose cold/warm bifurcation this retires), ADR 0003 (the VZ backend, migrated to real vsock
in Phase 2).

## Context

Confirmed live on prod session `20656e7c-…` (a `dev-brain` sandbox running `just dev`):
of the exposed ports, `:8080`/`:8082`/`:3001` (bound `0.0.0.0`/`::`) loaded, while `:3000`
(Vite) and `:10350` (Tilt UI) showed as down. `/proc/net/tcp` in the guest showed the exact
correlation: the failing ports were bound `127.0.0.1`; restarting Tilt with `--host
0.0.0.0` turned `:10350` green immediately.

The cause is structural. `crates/engram-host-agent/src/proxy_port.rs::open_tcp_tunnel_at`
opens `TcpStream::connect(guest_ip:port)` — from the host root netns for a cold sandbox, or
inside the per-VM netns (ADR 0044) for a warm-restored one. A connection arriving on the
guest's `eth0` cannot reach a listener bound to the guest's loopback. A port-preview feature
should reach the dev server the way it is actually served — as `localhost` *inside* the
guest — because that is where dev servers bind by default.

The host cannot reach the guest's loopback across the VM boundary. Something *inside* the
guest must make the `127.0.0.1` connection. `engram-agentd` (PID 1, the in-guest control
surface) is exactly that thing, and it already speaks vsock to the host for exec, the
harness channel, the forge bridge (ADR 0023), and artifact upload (ADR 0026).

## Decision

The host-agent stops dialing `guest_ip:PORT`. Instead it opens a vsock connection to a new
agentd **relay listener on vsock port 1030**, sends a one-frame header naming the target
port, and agentd dials `127.0.0.1:PORT` inside the guest and splices bytes. Only the guest
hop changes — the ADR 0064 tunnel above it (the coord `PortRelayService`, the host-agent
`ProxyPort` gRPC handler, the `pump_tcp_through_tunnel` byte pump, the 64 KiB host↔coord
framing, the orchestrator preview edge, and all auth/pinning) is untouched.

### Why vsock, not guest-side DNAT

The considered alternative was a guest-side redirect (`route_localnet=1` + an iptables
`PREROUTING -i eth0 -j DNAT --to 127.0.0.1`). Rejected: DNAT is the technique tools reach
for when they *cannot* run an in-guest agent — its whole value is agentless transparency,
which is moot here because we have agentd. It would make all inbound `eth0` traffic silently
redirect to loopback (a broad, implicit behavior change), depend on the `route_localnet`
footgun, couple its safety to the network topology staying point-to-point, and have to be
proven not to collide with the ADR 0006 egress NAT. The vsock relay instead (a) models the
operation explicitly, (b) is consistent with agentd being *the* in-guest surface (the
`guest_ip` dial is the one anomaly that bypasses it), (c) is topology-decoupled (vsock is
CID-addressed, not IP), and (d) **retires** code rather than layering NAT on top: the
`guest_ip`/netns dial and the cold/warm bifurcation both go away (the vsock UDS is a
host-root filesystem path, identical cold and warm).

### No head-of-line blocking (the hard requirement)

**One vsock stream per forwarded TCP connection; no application-level multiplexing.** Each
browser connection is already its own end-to-end chain (its own coord `Relay` gRPC stream →
its own host-agent `proxy_port` call → its own `PortTunnel`); we give it its own vsock
connection → its own agentd task → its own `127.0.0.1` dial. Independence is preserved at
every stage because every stage is a per-connection resource. The one shared layer is the
virtio-vsock device, and vsock's **per-socket credit-based flow control** saves us there: a
connection whose reader has stalled runs its own credit to zero and its bytes never reach
the shared virtqueue, so a stalled connection cannot occupy the device or starve others.

The single invariant that preserves this, stated so it cannot regress: agentd's accept loop
does `accept → tokio::spawn → (inside the task) read header, dial-with-retry,
copy_bidirectional`. It must never `await` the header read or the (retrying) `127.0.0.1`
dial on the accept path — one connection to a slow or refused port would otherwise block
every new preview stream for up to the 3 s retry budget. Corollaries: never mux connections
over one stream, never share a host-side vsock UnixStream across connections, hold no
per-sandbox lock on the data path.

### Throughput

The guest hop is built so it is never the bottleneck: `copy_bidirectional_with_sizes` with
256 KiB per-direction buffers (≥ vsock's ~64 KiB per-connection credit window), and
`TCP_NODELAY` on the loopback dial (loopback still runs Nagle × delayed-ACK, which stalls
small interactive writes — WS control frames, chunked-encoding boundaries). vsock loopback
RTT is tens of µs, so a 64 KiB window is ~1 GB/s/connection of headroom.

The end-to-end ceiling therefore sits **upstream**, at the gRPC HTTP/2 per-stream flow-
control window (~64 MiB/s for a single connection at intra-DC RTT); aggregate throughput
scales with parallel connections, which is the normal preview case (a browser opens many).
Widening the h2 windows / the `PortTunnel` channel is a shared-transport change with fleet-
wide blast radius (shell relay, exec, control plane) and is **explicitly deferred** to a
separate, measured change — the guest hop is built so no rework is needed if that lands
later. `SO_VM_SOCKETS_BUFFER_SIZE` is left at default (it governs host→guest upload credit,
not the hot guest→host direction); bump only if an upload benchmark shows starvation.

### Resource safety

A preview page opens many connections (HTTP/1.1 without keep-alive, plus WebSockets); a
malicious page could open thousands, each costing host + guest fds and an agentd task. The
coord enforces a **per-session concurrent-forwarded-connection cap** (default 256, matching
the host-agent gRPC `concurrency_limit_per_connection`, env-overridable); on exhaustion it
returns `resource_exhausted`, which the orchestrator maps to a clean **503** (WebSocket
**1013**). Agentd holds a matching defense-in-depth global cap (256) on its 1030 accept loop
— a guest serves exactly one session, so it's never reached in normal operation, and at 2
fds per connection it stays well under the default `RLIMIT_NOFILE` (no raise needed).

### Failure & lifecycle

`RelayAck{ok,error}` (not fire-and-forget) preserves ADR 0064's synchronous "dev server
down → clean 502" contract: agentd replies before splicing, and the host surfaces a dial
failure as a `proxy_port` error. The 1030 listener binds at agentd startup **before any
snapshot is taken**, so restored VMs are dial-ready. Pause/resume-in-place keeps live
tunnels; a restore onto a fresh FC process resets in-flight tunnels (the browser reconnects
— acceptable, and the shell pin exempts a live preview from *idle* eviction). Half-close and
teardown ride `copy_bidirectional`'s EOF handling + vsock RST. No relay auth in Phase 1:
host→guest is the trusted direction (unlike the token-gated guest→host forge/upload
bridges, where the guest is untrusted).

### Phases (each a PR on its own worktree)

- **P1 — production fix (Firecracker + Process).** harness-proto consts + `RelayConnect`/
  `RelayAck`; agentd `port_relay.rs` on 1030; a new `SandboxBackend::open_guest_stream`
  seam (FC reuses `connect_fc_vsock`; Process returns `None` → the host dials `127.0.0.1`
  directly, already correct since Process agentd is a host subprocess); host-agent
  `open_vsock_tunnel_at` + `proxy_port` rewrite (retiring `connect_cold`/`connect_in_netns`);
  the per-session cap; the `proxy_port_loopback` FC integration test (HOL + throughput)
  wired into CI. Fixes prod. **(Shipped #512.)**
- **P2 — VZ real vsock (shipped #514).** *(see "Phase 2 outcome" below.)* VZ's console-bridge
  vsock is single-stream-per-port (a persistent HMR WebSocket would starve other connections —
  HOL blocking). Migrate VZ onto Apple's `VZVirtioSocketDevice` (multi-stream) via `objc2`,
  retiring the console-bridge shim rather than forking a second mechanism, so macOS parity is
  HOL-free too.
- **P3 — shell unification.** Route `proxy_shell` through the same relay (ttyd binds a
  loopback port), retiring `connect_tcp_in_netns_linux`, the shell netns dial, and the
  cold/warm bifurcation — the code-retirement payoff. **(Shipped #513.)**

### Phase 2 outcome (VZ real vsock)

**The blocker that motivated the console swap was stale.** VZ had been migrated *off*
`VZVirtioSocketDevice` onto a multi-port virtio-console (commit `bf84dca2`) for exactly one
reason: "generic arm64 cloud-image kernels ship `CONFIG_VIRTIO_VSOCKETS` as a *module*, which
can't auto-load before init runs." But the kernel VZ actually boots is the **Kata static arm64
kernel** (`just pull-kernel` → Linux 6.12.28). Extracting its embedded `IKCONFIG` shows
`CONFIG_VSOCKETS=y`, `CONFIG_VIRTIO_VSOCKETS=y`, `CONFIG_VIRTIO_VSOCKETS_COMMON=y` — all
**built-in**. So real vsock works on the guest we ship, and the console detour (single stream
per port ⇒ HOL blocking on the relay) was buying nothing. Phase 2 reverts to real vsock and
retires the console mechanism entirely.

**What landed.** `vm.rs` attaches a `VZVirtioSocketDeviceConfiguration` in place of the
multi-port `VZVirtioConsoleDeviceConfiguration` (the kernel-log serial console is untouched).
A new `vsock_bridge.rs` (adapted from the pre-`bf84dca2` `vsock_bridge.rs`, whose delegate
plumbing objc2 0.6 still supports verbatim) provides:

- a `VsockConnector` (device + queue handle) that dials host→guest ports via
  `connectToPort:completionHandler:` and hands back the connection fd as an
  `AsyncRead+AsyncWrite` stream — with a boot-race/post-restore retry;
- the **agentd port (1024)** as a host `UnixListener` that `connectToPort`s per accept
  (preserving the backend's `UnixStream::connect(<base>_1024)` surface — zero churn in
  `start_agent`/`exec_stream`/`start_shell`/`guest_ip`), but now one vsock stream per accept,
  so concurrent control RPCs no longer serialise;
- **guest→host listeners** on harness (1026) and upload (1029) via a `VZVirtioSocketListener`
  delegate (`define_class!`), each accepted connection's fd handed *directly* to the sink —
  no duplex hop;
- **`SandboxBackend::open_guest_stream`** (the P1 seam VZ inherited as `None`): dials guest
  vsock 1030 through the `VsockConnector` and returns `Some(stream)`, flipping VZ off the
  `guest_ip`/eth0 fallback onto the relay. Each forwarded browser connection is its own
  `connectToPort` stream and `VZVirtioSocketDevice` muxes them freely — **HOL-free, matching
  FC.**

**Divergences / decisions:**

- **Console retired end-to-end, not just the host bridge.** Because the guest side keyed off
  `ENGRAM_TRANSPORT=console`, retiring the host `console_bridge.rs` also meant retiring the
  in-guest `engram-transport::ConsoleTransport`, the `image-builder`/`engram-cli`
  `Transport::Console` variant, the `Transport::supports_ready_port` trait method (console was
  its only `false` case), and the `ENGRAM_DIAG_NO_CONSOLE` diagnostic. `bake-demo.sh` now
  bakes `ENGRAM_TRANSPORT=vsock` for every backend. **A VZ image baked before this change
  (carrying `ENGRAM_TRANSPORT=console`) will not boot against the vsock-only backend — re-bake
  is required.** Consistent with the zero-users clean-break policy; the `Transport` enum is
  left as a single-variant seam rather than churning ~15 FC test call sites.
- **Ready port (1027): drain, don't gate.** On the vsock transport, agentd dials
  `ENGRAM_AGENTD_READY_PORT` at startup and writes one fire-and-forget `AgentReady` frame; with
  no host listener it would spin ~90 s before serving RPCs. VZ registers a *draining* listener
  on 1027 (reads to EOF and drops) so the guest handshake completes on the first dial. This
  keeps VZ's existing "first read on the agentd UDS blocks until agentd binds" readiness model
  rather than adopting FC's `wait_agent_ready` *gating* — a smaller, lower-risk change;
  FC-parity gating is a possible future enhancement. (The agentd handshake loop dropped its
  `supports_ready_port()` guard accordingly — vsock always has a listener now.)
- **Upload simplified to FC's shape.** With real vsock each upload is its own connection, so
  the console bridge's byte-delimited `upload_pump` serialisation is gone; the upload sink now
  drives one connection per upload exactly like FC.
- **Bootstrap port (1025) retired** — it was dead on VZ (never dialed by the host, never bound
  by the guest, which only listens on 1024).

**Pitfalls found in hardware validation** (the live-VM path was authored blind — none of these
are visible to `cargo test`/CI, which can't boot a VZ VM):

- **vsock UDS paths overflow `SUN_LEN`.** The per-port host UDS (`<base>.vsock_<port>`) was
  rooted under the backend's `work_dir`. On macOS `sockaddr_un.sun_path` is 104 B, and a deep
  `work_dir` (a git-worktree checkout, a long `$HOME`, or `$TMPDIR` = `/var/folders/…`) plus the
  36-char sandbox-UUID filename overflows it — `bind` fails with "path must be shorter than
  SUN_LEN" before the VM even boots. Pre-existing (the pre-`bf84dca2` vsock used the same scheme;
  the console detour bound no UDS and masked it) and it hits real `just dev`, not only tests.
  Fixed by rooting the socket in a short `/tmp` dir via a new shared
  `engram_core::socket::short_socket_dir()` — one tested SUN_LEN-safe primitive the FC
  integration tests now share too (retiring their ad-hoc `/tmp` copy).
- **`VZVirtioSocketListener::setDelegate:` is a weak property.** The guest→host listeners
  (ready 1027, harness 1026, upload 1029) attached a delegate but the bridge stored only the
  *port numbers* — the delegate objects dropped after registration, so their weak refs went nil
  and `shouldAcceptNewConnection:` was never called. Every guest-initiated connection was
  silently rejected: agentd's ready-port dial never landed, so it spun its full 90 s deadline
  before serving RPCs (host→guest worked fine, so exec/`guest_ip` eventually succeeded — just
  ~90 s late, tripping the 60 s `await_agent`). Fixed by holding the listeners + delegates alive
  on the `VsockBridge` for its lifetime. (The pre-`bf84dca2` bridge kept a `Retained` ref; the
  re-port lost it — this is *why* the "drain, don't gate" ready handshake above needs the
  delegate to actually fire.)
- **The demo image has no `node`.** The relay test started its loopback echo/black-hole servers
  with `node -e`, but `deploy/demo` is `debian-slim` (git/curl/ttyd only). Switched to `socat`
  (matching the FC `proxy_port_loopback` test) + `ip link set lo up`, and added `socat`/`iproute2`
  to the demo image.

**Validated on hardware:** `just vz-codesign` + a freshly-baked vsock `ENGRAM_VZ_ROOTFS`, then
`cargo nextest run -p engram-sandbox-vz --run-ignored ignored-only` — `e2e_vz_lifecycle` (core
real-vsock lifecycle) and `e2e_vz_port_relay_reaches_loopback_without_hol` (the relay +
HOL-freedom property) both pass on a live VZ VM. Plus `cargo clippy` (VZ native +
`engram-core`/FC on `aarch64-unknown-linux-musl`, `-D warnings`), `cargo fmt`, and unit tests.
The two `e2e_vz` tests stay `#[ignore]` — they can't run in CI (the macOS runner has no Docker
to bake a rootfs, the gap ADR 0032 tracks), so they serve as a local pre-merge gate.

## Consequences

- Loopback-bound dev servers work with no per-app `--host 0.0.0.0` (that workaround stays
  valid but stops being required).
- The `guest_ip`/netns dial and the cold/warm split are retired for ports (P1) and for the
  shell (P3); agentd gains one small relay service.
- Requires a re-baked guest image carrying the new agentd. A pre-change snapshot / old image
  has no 1030 listener; on FC the `CONNECT 1030` fails cleanly → `proxy_port` errors → a
  clean "preview unavailable" (graceful degradation, no crash). Roll new agentd before
  relying on the relay.
- No wire/proto change (the vsock connect is a host-local `SandboxBackend` call); no schema
  change.
