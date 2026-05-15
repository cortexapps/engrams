# ADR 0013: Stateless dial-on-demand transport (coord ↔ host)

Status: accepted, 2026-05-15
Phase: 1 (in flight) — `hosts.host_addr` column landed; trait surface
changes + gRPC server / pool / coord HTTP endpoints to follow.

## Context

The coord ↔ host transport today is a single long-lived WebSocket per
host-agent, pinned to one coord pod by the L4-LB's hash. Every
coord→host RPC (CreateSandbox, ExecStart, Snapshot, …) and every
host→coord message (Heartbeat, ResolveRegistryAuth, HarnessEvent)
flows over that one connection.

Three consequences forced this ADR:

1. **Coord can only run single-replica today.** ADR 0012 (deferred)
   captures the cross-pod scheduling failure: existing-session ops
   (exec, prompt, snapshot) need to reach the host whose WS lives on
   exactly one pod, so any other pod returns "no hosts connected"
   half the time. `engrams-internal/values/engrams.yaml` runs at
   `replicaCount: 1`, HPA + PDB disabled — Slurm-shaped active-single
   control plane.
2. **WS reconnect storm on coord rolling restart.** Every host
   disconnects + reconnects with exponential backoff; in-flight RPCs
   error; the API outage window is the ~30 s tail of the reconnect
   loop.
3. **Bincode-over-WS is brittle.** Schemaless, positional. A single
   mismatched int silently misaligns every byte that follows
   (`crates/engram-protocol/src/wire.rs:24-30`). `WIRE_VERSION` gating
   makes mid-rollout skew fail loudly instead of silently, but the
   underlying fragility is structural.

The research survey (see `~/.claude/plans/cheeky-booping-castle.md` for
the long-form) confirmed every fast OSS dev-env that ships horizontal
control planes (E2B's `infra` repo, Lambda's MicroManager,
CodeSandbox's described stack, fly.io's Machines) shares one shape:
**stateless API replicas + per-host autonomy**. E2B specifically dials
every host on demand from every API replica using a
`sandbox_id → host_addr` lookup against shared state; placement is
local Best-of-K with optimistic accounting; no leader election. There
is no per-replica WS pinning to fix because there are no per-replica
long-lived connections.

The chunked-storage data path
(`crates/engram-host-agent/src/pooled_backend.rs:251-341` →
`engram-chunk-store` → GCS/OCI Range-GET → UFFD/NBD → guest) is
**completely orthogonal** to the coord ↔ host transport. The coord is
never on a chunk-byte path. Transport-layer work here cannot regress
chunk-fetch latency.

ADR 0011 already factored the coord-side abstraction along the right
axis: `HostClient` is the transport seam, with `LocalHostClient`
(in-proc, mode=all) and `RemoteHostClient` (WS, the WS-only impl
today). Swapping the remote impl is the move; everything else above
the trait stays.

## Decision

Replace the WS with:

- **gRPC over HTTP/2** for coord → host (12 unary RPCs + 1 streaming),
  using `tonic` (0.12, already in workspace `Cargo.toml:97-100`).
- **Plain HTTP/JSON** for host → coord (register, heartbeat,
  registry-auth, harness events, idle-eviction candidates), served by
  the existing axum router.

Coord pods become stateless. Any pod can dispatch any op to any host
via a per-pod `Arc<GrpcHostPool>` keyed by `HostId`, looking up
`host_addr` from the `hosts` table (cached pod-locally for 30 s). Any
host can heartbeat / register / push events to any pod through the
same L4-LB that serves user API traffic.

The split is deliberate:

| Direction | Why this transport |
|---|---|
| **coord → host** (gRPC) | Strongly-typed schemas eliminate the bincode silent-skew failure mode. HTTP/2 multiplexes many concurrent RPCs over one TCP+H2 connection per host with no head-of-line blocking — `ExecStart` server-streaming gets its own H2 stream while a unary `CreateSandbox` on the same connection completes independently. `tonic` already lands the dependency. |
| **host → coord** (HTTP/JSON) | Connectionless POSTs that fan into any coord pod through the existing LB. No need for streaming or strong typing here — heartbeat is small, events are one-per-POST and de-dup via the existing `(idx, event)` monotonic key in `session_events`. Reusing axum + `reqwest` avoids forcing a second `tonic` server on every coord pod. |

### `HostService` gRPC schema

`crates/engram-protocol/proto/host_service.proto` (sketch in the
implementation plan). One method per existing `HostClient` trait
method:

- Unary: `CreateSandbox`, `DestroySandbox`, `ListSandboxes`,
  `Snapshot`, `Restore`, `GuestIp`, `BindHarnessSession`,
  `UnbindHarnessSession`, `SendHarnessPrompt`, `StartAgent` (with
  bundled policy, see below), `ReapMaterializeDir` (folded in — see
  ADR 0011 follow-up #1), `AcquireShell`, `ReleaseShell` (ADR 0011
  follow-up #3), `Ping` (warm-up).
- Server-streaming: `ExecStart` returning `stream ExecFrame { Started
  | Stdout | Stderr | Exit }`.

For complex payload types already serde-derived in `engram-core`
(`SandboxSpec`, `SnapshotMetadata`, `SessionEgressPolicy`,
`AgentSpec`), the proto message carries a single `bytes` field holding
the type's bincode encoding. This keeps the proto layer small while
preserving Rust-side type safety. Simple values (`sandbox_id`,
`min_age_secs`, etc.) are native proto fields.

### Host → coord HTTP endpoints

| Endpoint | Method | Replaces |
|---|---|---|
| `/api/hosts/register` | POST | `NotifyKind::Hello` (`api/hosts.rs:178-212`) |
| `/api/hosts/:id/heartbeat` | POST | `NotifyKind::Heartbeat` + `HeartbeatAck` |
| `/api/hosts/:id/auth/resolve-registry` | POST | `RequestKind::ResolveRegistryAuth` + `AuthRequestHandler` |
| `/api/sessions/:session_id/harness-events` | POST | `NotifyKind::HarnessEvent` forwarding |
| `/api/hosts/:id/idle-eviction-candidates` | POST | new — host pushes candidates instead of coord polling local hub (ADR 0011 follow-up #2) |

Bearer-token auth on all five via the existing middleware.

### Ordering: `SessionEgressPolicy` + `StartAgent`

Today's guarantee that the policy notify arrives before `StartAgent`
comes from WS frame order. With stateless dispatch there is no
per-host single connection, so frame ordering disappears.

**Fix: bundle the policy into `StartAgent`.**

```protobuf
message StartAgentRequest {
  bytes sandbox_id = 1;
  AgentSpec agent = 2;
  SessionEgressPolicy egress_policy = 3;
}
```

The host's handler applies the policy to its egress proxy registry
**before** spawning the agent — atomic by construction. `HostClient`
trait signature gains the policy parameter; `notify_session_policy`
is removed from the trait.

### Connection pool

Each coord pod owns `Arc<GrpcHostPool>` — a
`DashMap<HostId, PooledConnection>` of `tonic::transport::Channel`
instances. One `Channel` per host = one TCP+H2 connection
multiplexing N concurrent streams. Config:

- HTTP/2 keepalive 15 s / 5 s timeout
- Connect timeout 2 s
- Idle close 5 min
- TLS off (plaintext h2c inside VPC; bearer token is the auth boundary,
  the firewall is the network boundary; same trust model as the WS
  path)
- Max concurrent streams 256 (host-side)

Pre-dial:

1. On `/api/hosts/register` receive, the receiving pod warms its pool
   entry and fires one `Ping` to force the TCP+H2 handshake.
2. On every heartbeat (any pod), warm the pool idempotently —
   refreshes `last_used_at`, keeps the channel from idle-closing.
3. On coord startup, each pod reads `SELECT id, host_addr FROM hosts
   WHERE status IN ('ready','draining')` and warms its pool — ~10 ms
   per host.

Worst case: a pod that has never seen heartbeat-or-register for host X
gets a session-create routed to it before warming. One-time ~3 ms
cold-dial penalty in the VPC. p99 warm-RPC latency target ≤ 5 ms,
matching today's WS frame.

### State migration

#### `hosts.host_addr` column

Migration `deploy/migrations/0024_hosts_host_addr.sql`:

```sql
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS host_addr TEXT;
```

`HostRecord` gains `pub host_addr: Option<String>`. Pre-0013 rows
keep `NULL` and are unreachable via gRPC until they re-register; the
coord's dispatch path treats `NULL host_addr` as unreachable. Upsert
on register / heartbeat preserves the addr (`COALESCE`).

#### `sandbox_owner` lookup

Today: in-memory `DashMap<SandboxId, HostId>` on the WS-owning pod
(`host_registry.rs:73,287-297`). Tomorrow: read from `sessions.host_id`
JOIN `hosts.host_addr` on dispatch, with a 30 s pod-local TTL cache.
Cross-host migration is rare + ADR-bound, so staleness is safe. New
creates seed the cache synchronously from the call result so
immediate `StartAgent` doesn't bounce through PG.

#### Dead-host detection

Unchanged — already SQL-driven
(`crates/engram-coordinator/src/dead_host.rs:89-203`).
`pg_notify('host_dead', ...)` drives per-pod pool eviction via the
existing `pg_listener`; pods holding a `Channel` to the dead host
drop it; in-flight gRPC errors with `tonic::Code::Unavailable`.

## Consequences

- **Coord runs N replicas.** No leader election, no shared in-memory
  state on the dispatch path. `engrams-internal/values/engrams.yaml`
  goes back to `replicaCount: 3`, HPA + PDB re-enabled. ADR 0012
  moves from `deferred` → `landed via 0013`.
- **Coord rolling restart is invisible to hosts.** Host-agents have
  no persistent connection to any specific pod; heartbeats route
  through the L4-LB to any live pod. In-flight gRPC unaries during
  a pod restart fail with `Unavailable` and retry transparently via
  the pool's retry shim; no 30 s outage window.
- **Wire schema is now versioned at the protobuf layer.** Drift fails
  loudly at the gRPC layer instead of silently misaligning bytes.
- **ADR 0011's `HostClient` trait survives unchanged in shape.** The
  remote impl swaps from `RemoteHostClient` (WS) to `GrpcHostClient`
  (tonic Channel). `LocalHostClient` (mode=all) is unaffected.
- **The ADR 0011 follow-ups land in this work.** ADR 0011 flagged
  three "out of scope" items that are actually prerequisites for
  stateless correctness:
  - `HostAdminHandler` → folded into `HostService` (one trait now).
  - `idle_evictor` driver moves to the host-agent (the local hub is
    the authoritative idle source); the pipeline
    `evict_idle_session` stays coord-side. Hosts POST candidates to
    `/api/hosts/:id/idle-eviction-candidates`.
  - `api/shell.rs::acquire_shell`/`release_shell` routes through
    `HostClient` instead of touching `state.harness_hub` directly,
    so calls reach the host owning the harness session.
- **Bincode dependency disappears from coord↔host.** `engram-protocol`
  retains the `heartbeat::Heartbeat` / `HeartbeatAck` types (now
  JSON body shapes for the HTTP heartbeat endpoint) and deletes
  `client.rs`, `server.rs`, `codec.rs`, `wire.rs`. `tokio-tungstenite`
  drops from the workspace.
- **Two listeners on every host-agent.** 9100 (Prometheus + GCE MIG
  TCP health check, unchanged) and 9101 (gRPC). Intra-VPC firewall
  already permits all TCP
  (`deploy/terraform/gcp/modules/network/main.tf:56-73`); no TF
  change.
- **Host registration becomes explicit.** Host-agent reads its own
  internal IP from GCE metadata at startup, POSTs
  `/api/hosts/register` once, then starts the gRPC server +
  heartbeat loop. On GCE MIG instance replacement the new instance
  registers with a fresh addr; old `hosts` rows are reaped by the
  existing dead-host detector.
- **Mode=all is unchanged.** Single process, single `LocalHostClient`,
  no network. The plan-time wiring construction in
  `crates/engram-coordinator/src/main.rs` registers the in-proc
  client through `HostRegistry` as today.

## Alternatives considered

- **Pull-based dispatch (ADR 0012's original recommendation).**
  Hosts watch a `sandbox_assignments` table; coord writes assignments
  rather than dispatching. Rejected as the *general* shape because
  the dispatch overhead (pg_notify + LISTEN delivery + per-assignment
  SELECT) is 10-30 ms vs gRPC's <5 ms warm, eating 10-30% of the
  100 ms cold-start budget. Pattern is still useful for cross-pod
  reconciliation and may resurface for specific flows later.
- **Centralized routing layer (Lambda's WorkerManager).** A separate
  service routing `sandbox_id → coord-pod-that-owns-WS`. Rejected:
  introduces a new component to work around a problem we created by
  pinning. Stateless dispatch removes the problem.
- **Keep the WS, add leader election (Slurm shape).** What ADR 0012
  effectively prescribed with single-replica. Acceptable as a holding
  pattern but caps coord throughput at one process forever; the
  whole reason to do this work is to lift that ceiling.
- **gRPC bidirectional streaming for host→coord traffic too.**
  Rejected: re-introduces pinning. Each long-lived stream attaches
  to one pod. The whole point is no pinning. Per-event POST fans
  into any pod through the LB; the events table + `pg_listener` is
  the cross-pod fan-out primitive we already have.
- **HTTP/JSON for coord→host instead of gRPC.** Considered. Loses
  server-streaming for `ExecStart` (would need SSE bolt-on), loses
  schema versioning, loses HTTP/2 stream priority. The complexity
  budget is roughly the same. gRPC wins on streaming semantics
  alone.

## Out of scope (follow-ups)

- **mTLS / SPIFFE inside the VPC.** Plaintext h2c is sufficient for
  v1; bearer token + firewall match today's WS trust boundary. If
  the threat model changes, `tonic::transport::ClientTlsConfig`
  drops in without further architectural work.
- **Pre-warmed paused-VM pool + working-set prefetch.** The
  research-survey conclusion was that these — not transport — are
  what move the 100 ms cold-start number. Separate ADR.
- **`Ping` doubling as the GCE MIG TCP health check.** Could let us
  drop port 9100. Out of scope; the metrics listener has its own
  reasons to live.
- **Reconnect-with-replay semantics for `ExecStart`.** Today a coord
  pod restart kills in-flight exec streams. We can let the SPA
  resume by exec-id (the host knows the stream and can re-attach a
  new gRPC channel), but the flow needs design work.

## Implementation

See `~/.claude/plans/cheeky-booping-castle.md` for the
commit-by-commit plan. Landing order:

1. ✅ Migration `0024_hosts_host_addr.sql` + `HostRecord.host_addr`
   field + postgres adapter updates. Existing WS path unaffected.
2. `HostClient` trait surface changes (`start_agent` bundles policy,
   `notify_session_policy` removed, `acquire_shell` /
   `release_shell` / `idle_eviction_candidates` added).
3. Proto + `tonic-build` scaffolding.
4. `GrpcHostClient` + `GrpcHostPool` (dark — nothing wired yet).
5. `grpc_server` + `coord_client` in host-agent.
6. New HTTP endpoints in coordinator/api/.
7. `host_registry` rewrite to dispatch through pool.
8. `idle_evictor` split (driver out, pipeline stays).
9. Delete WS code; drop `tokio-tungstenite`.
10. Integration tests + dispatch-latency bench.
