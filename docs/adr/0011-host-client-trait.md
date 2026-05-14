# ADR 0011: `HostClient` — separating coord↔host transport from the VMM dimension

Status: accepted, 2026-05-13
Phase: 1 (landed) — `HostClient` trait, `LocalHostClient` +
`RemoteHostClient`, harness-event forwarding, and the
`StartAgent`/`BindHarnessSession`/`UnbindHarnessSession`/`SendHarnessPrompt`/
`GuestIp`/`HarnessEvent` wire variants.

## Context

`SandboxBackend` (`engram-core::traits::sandbox`) was the only trait
the coordinator imported, with `RemoteSandboxBackend`
(`engram-protocol::client`) wrapping a WS connection to satisfy the
same trait for a remote host. That single-trait shape worked when
`--mode=all` was the only configuration that needed harness routing.
It broke down once `--mode=coordinator + --mode=host` became the
target topology.

The breakdown wasn't a single bug — it was a pile of small
asymmetries that all traced back to one design issue: `SandboxBackend`
was carrying two orthogonal abstractions.

1. **The VMM dimension.** What kind of VM is this — Firecracker, VZ,
   Process? Decides `create` semantics, `snapshot` shape, the
   in-VM agent transport. Local-only by definition; closures and
   file paths don't cross a network.

2. **The transport dimension.** Where does the call land — in this
   process, or on a host on the other end of a WS? Decides whether
   `notify_session_policy` is a method call or a wire Notify;
   whether `start_agent` runs synchronously or after a unary RPC.

Tangling them produced specific failures the moment split mode
came up:

- **`start_agent` defaulted to a "not supported yet" error** so
  `RemoteSandboxBackend` (which didn't implement it) could compile.
  Result: harness sessions in `--mode=coordinator` returned
  `bad_request: "this backend doesn't support start_agent yet"`
  even though every backend on the host side supports it.

- **`set_harness_sink(sink: HarnessSink)`** takes a `Fn(stream) → ()`
  closure capturing coord-side state. It can't serialize. The trait
  pushed the no-op on `RemoteSandboxBackend` quietly; every vsock
  dial from the harness inside a guest got dropped with
  `"harness connection arrived but no sink registered; dropping"`.

- **`snapshot_path_for(snapshot_id) → PathBuf`** returns a
  host-local filesystem path. The remote backend was forced to
  invent a sentinel (`/__engram_remote_backend_no_local_path__`)
  because the trait method had to exist; calling it from the coord
  side was a programmer error that the compiler had no way to flag.

- **`HarnessHub` was a concrete struct on `SharedState`** that
  every coord-side `bind_session` / `unbind_session` / `send_prompt`
  call reached into directly. There was no abstraction for "the
  hub that owns the sandbox in question" because the assumption
  was "the hub is here, in this process." In `--mode=coordinator`,
  the hub's `connections` map was always empty (the FC backend's
  vsock accepts happened on a different process).

Five workarounds in a trait that conflated two dimensions; nothing
that read like a single bug.

## Decision

**Split the two abstractions into separate traits, and have the
coord talk to exactly one of them.**

### `SandboxBackend` (`engram-core::traits::sandbox`)

Narrowed to "the local VMM driver." Three production impls
(`engram-sandbox-firecracker::FirecrackerBackend`, `engram-sandbox-
vz::VzBackend`, `engram-sandbox-process::ProcessBackend`) plus test
fakes. Everything that's host-local stays here: `snapshot_path_for`,
`set_harness_sink`, the vsock-based `start_agent` and `exec_stream`,
the static `/30`-derived `guest_ip` fast-path. The coord does not
import this trait. Future VMMs (Cloud Hypervisor, raw libkrun, Kata)
add one impl and inherit the rest.

### `HostClient` (`engram-core::traits::host_client`)

New trait. The coord↔host boundary. Two impls:

- **`LocalHostClient`** (`engram-host-agent::host_client`) —
  composes `Arc<dyn SandboxBackend>` + `Arc<HarnessHub>` and
  delegates each method to the right inner thing. Used in
  `--mode=all` (the coord constructs one after `AppState` is built
  so the hub it owns is the same hub the FC backend's harness
  sink writes into) and inside the host-agent itself (it's what
  `engram-protocol::server` serves over the WS).

- **`RemoteHostClient`** (`engram-protocol::client`, renamed from
  `RemoteSandboxBackend`) — wraps a `ConnectedHost` (WS). Every
  method is a unary RPC. The `snapshot_path_for` sentinel is gone.

`HostRegistry` (`engram-coordinator::host_registry`) stores
`Arc<dyn HostClient>` per host and itself implements `HostClient`,
dispatching by `sandbox_id → host_id`. The rest of the coord
routes through one type — `state.services.host: Arc<dyn HostClient>`
(was `state.services.sandbox: Arc<dyn SandboxBackend>`).

### Wire surface

`engram-protocol::wire::RequestKind` gains:
- `StartAgent { sandbox_id, agent }` — was the immediate trigger;
  `RemoteSandboxBackend` had inherited the trait's "not supported"
  default and blocked harness sessions outright.
- `BindHarnessSession { session_id, sandbox_id }` /
  `UnbindHarnessSession { session_id }` /
  `SendHarnessPrompt { sandbox_id, text }` — the three harness
  routing methods now go through the wire. Hub state lives on the
  host that owns the connection.
- `GuestIp { sandbox_id }` — the coord asks the host for the
  guest's IPv4 before building `SessionEgressPolicy`. Without
  this, the policy was never registered in the host's egress
  registry (the egress proxy's allow-list keys on guest IP).

`ResponseKind` gains `AgentStarted`, `HarnessOk` (generic ack for
the three harness ops), and `GuestIp { ip: Option<String> }`.

`NotifyKind` gains `HarnessEvent { session_id, sandbox_id, event,
at }` — host → coord. The host's local `HarnessHub` runs its
`EventSink` for every per-session event; in `--mode=host` the sink
ships the event over the WS instead of writing to the coord's
session_events log directly. The coord's `api/hosts.rs` read loop
replays each forwarded event through its in-proc hub via
`HarnessHub::emit_external`, so SSE subscribers see the same
stream they would in `--mode=all`.

`SessionEgressPolicy` already existed as a `NotifyKind`; this work
fixed the host-side `handle_notify` dispatch so the policy
actually reaches `backend.notify_session_policy(policy)` instead
of being logged as "unexpected SessionEgressPolicy; ignoring."

## Consequences

- **The harness path works end-to-end in split mode.** Bogus-key
  Claude session in `--mode=coordinator + --mode=host` surfaces the
  Anthropic auth-error text as a `role=assistant` `agent_message`
  on the coord's SSE stream. Before this work it failed
  `bad_request` at `start_agent`.

- **The coord doesn't import `SandboxBackend` anywhere.**
  `cargo tree -i engram-core::traits::SandboxBackend` shows only
  the VMM crates + the host-agent. Future "what does the coord need
  to know about VMs" questions stay scoped to the `HostClient`
  surface.

- **One trait, one place per dimension.** Adding a new VMM is one
  `SandboxBackend` impl. Adding a new transport is one `HostClient`
  impl. Adding both (a hypothetical "VM-on-IPC" backend) is one of
  each; before, you'd have had to either tangle them in a single
  trait impl or invent a parallel trait surface.

- **`RemoteHostClient::guest_ip` now has a real return value.** The
  coord can read the guest's IP across the wire. This was the
  blocking gap for the DNS filter (ADR 0010) — without it,
  `notify_session_policy` skipped the registration step and the
  egress proxy registry was always empty in split mode.

- **`handle_notify` now takes `Arc<dyn HostClient>` alongside the
  `Option<Arc<dyn NotifyHandler>>`.** Notifies that are "push this
  state to the local backend" (`SessionEgressPolicy`) dispatch to
  the backend directly; notifies that are "tell the coord about
  something" (`Hello`, `Heartbeat`) go through the handler. The
  earlier shape — where every Notify went through a single
  handler — couldn't reach the backend without the handler
  re-exposing it, which is the same coupling problem solved one
  level up.

- **The mode=all "in-process bypasses the WS path" framing is
  gone.** Both modes construct `LocalHostClient(PooledBackend,
  HarnessHub)` and register it through `HostRegistry`; the
  difference is whether `HostRegistry` is talking to the in-proc
  client or a remote one over the wire. Symmetric.

## Out of scope (follow-ups)

- **`idle_evictor`** still reads `state.harness_hub.idle_sandboxes(...)`
  directly. The right shape is to move the eviction driver to the
  host-agent (the hub state is authoritative there) and have the
  host push candidates via a new `NotifyKind::IdleEvictionCandidates`.
  Tracked in `docs/known-issues.md#8`.

- **`api/shell.rs`** still touches `state.harness_hub` directly for
  `acquire_shell` / `release_shell`. Two more HostClient methods.

- **`HarnessHub` lives in `engram-host-agent`** so `engram-core`
  doesn't grow a transport dependency. The coord still imports
  it for its in-proc hub construction in `--mode=all`. A future
  cleanup could move the hub type into `engram-core` if the
  coord's mode=all wiring sprouts a cleaner construction site;
  for now the dep direction is consistent (host-side glue uses
  host-agent's types).

- **`HostAdminHandler`** (`engram-protocol::server`) is still a
  separate trait for materialize-dir reap fanout. It could fold
  into `HostClient`; left as-is because the admin RPC is one
  method and the trait fan-out is cheap.

## Alternatives considered

- **Keep `SandboxBackend`, add a parallel `HarnessClient` trait
  for the harness ops.** Rejected: two traits the coord routes
  through, two registries, two sets of WS round-trips. The
  duplication doesn't pay for itself when both traits dispatch by
  `sandbox_id → host_id` anyway.

- **Make `RemoteSandboxBackend` implement `start_agent` as a unary
  RPC and leave everything else as-is.** That was the original
  shape — and we landed `StartAgent` over the wire as the first
  step before noticing the rest of the asymmetries. The
  cumulative weight of `set_harness_sink`, `snapshot_path_for`,
  and the harness routing made "one more method on the existing
  trait" the wrong long-term answer.

- **Skip the trait split entirely and just dispatch on the
  `SandboxBackendChoice` enum at every call site in the coord.**
  Rejected on aesthetic + symmetry grounds: the coord shouldn't
  care which VMM a session ran on; the boundary is "this host"
  vs "that host," not "FC" vs "VZ."

## Implementation

Landed across five commits in May 2026:

- `feat(protocol): StartAgent over the wire` (`8597409`) — first
  half of the harness gap. The wire variant + dispatch.
- `refactor: introduce HostClient trait for the coord↔host
  boundary` (`4d10ef9`) — the trait, both impls, the registry
  migration, the coord call-site rename, three new wire variants
  (`BindHarnessSession` / `UnbindHarnessSession` /
  `SendHarnessPrompt`).
- `feat(host-agent): forward harness events to coord over WS`
  (`92ca4b9`) — host-side `HarnessHub` + `NotifyKind::HarnessEvent`
  + coord ingest.
- `chore: post-Phase-2 cleanups + demo manifest network widening`
  (`781f571`) — comment audit, `WIRE_VERSION` reset.
- `fix(egress): make split-mode FC actually route DNS through the
  proxy` (`120b16e`) — including `GuestIp` over the wire,
  `handle_notify` dispatching `SessionEgressPolicy` to the
  backend, and the rest of the host-agent wiring needed for split
  mode to actually function.
