# Engram — Design & Plan

A self-hosted, open-source orchestrator for ephemeral AI agent sandboxes. Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, snapshot lifecycle (Firecracker's UFFD-backed restore), multi-host scheduling, and pluggable cloud / blob-storage backends. A subprocess-based dev backend lets the entire orchestration layer run on macOS Apple Silicon during development; production isolation is always Firecracker.

> Project lives in a brand-new repository. This document is the founding design + roadmap.

---

## Context

We want a Modal-style sandbox-as-a-service for AI coding agents (think Stripe's Minions, Ramp's Inspect) — but **open-source and self-hostable**, so any organization can run their own without vendor lock-in.

The unsolved gap: existing open-source primitives (Firecracker, Cloud Hypervisor, libkrun, E2B's infra repo) give you per-VM mechanics. Nobody ships the **orchestration layer above the VM** in a clean, portable way: warm pools, snapshot tiering, multi-host scheduling, cloud-backend abstraction, spot/preemptible eviction handling. Engram fills that gap, on top of Firecracker.

Cortex (our org) runs on GCP. Other adopters will run on AWS, Hetzner, k8s, or bare metal. Engram is built **GCP-first but cloud-agnostic**: all cloud-specific surfaces live behind traits with stub implementations for non-GCP backends shipped from day one.

The architecture comes out of an extended design discussion that explored: Stripe Minions / Ramp Inspect / Modal mechanics, single-box vs multi-host scaling, snapshot lifecycle, K8s vs raw VMs, spot tolerance, and graceful failure. This document captures the conclusions.

---

## Goals

1. **Sub-second sandbox spawn** for warm-pool checkouts (cold starts hidden behind a pre-warmed pool of microVMs per repo).
2. **Snapshot-evict mechanic** for time-sharing host RAM across more sessions than fit at once.
3. **Pluggable cloud backend** so the project ports cleanly between GCP, AWS, Hetzner, and self-hosted bare metal.
4. **Pluggable storage backend** (GCS, S3, MinIO, local) for snapshot durability.
5. **Spot/preemptible-tolerant** by default — eviction = forced snapshot + resume elsewhere, not data loss.
6. **Recoverable from snapshot loss** without losing user work (Postgres + git remain sources of truth).
7. **Single-binary single-host** mode for trivial deployment; **multi-host** mode when scale demands.
8. **Open-source, Apache-2.0**, idiomatic Rust, well-tested, contributable.

## Non-goals

- **Not an agent harness.** Engram hosts sandboxes; the LLM agent runs inside. Out-of-scope to wrap any specific harness (claw-code, OpenCode, Goose, etc.) — Engram exposes a generic execution API.
- **Not a CDE for humans.** No code-server, no VS Code plugin, no human IDE workflows. Engram is for unattended/programmatic use. Other tools (Coder, DevPod) handle the human case.
- **No multi-tenancy hardening for untrusted code in v1.** Sandboxes are isolated VMs but we assume tenants are trusted (single-org deployment). True multi-tenant isolation comes later if at all.
- **No K8s-native integration in v1.** K8s for the control plane is fine (just deploy the binaries as pods); putting microVMs *inside* K8s pods is explicitly out of scope (architectural mismatch covered in design discussion).

---

## Background (one paragraph)

The Modal/E2B/Ramp pattern: spawn an ephemeral, isolated environment per task; pre-warm a pool so cold starts feel instant; snapshot the FS+memory state when sessions go idle; evict idle sessions from RAM and resume from snapshot when they come back. This pattern is what makes "1000+ unattended PRs/week" economically viable — RAM is the binding constraint, and snapshot-evict is the load-bearing trick that lets you multiplex N×more sessions onto fixed hardware. Engram brings this to open source with a clean architecture.

---

## Architecture overview

```
                  ┌──────────────────────────────┐
                  │   Engram Coordinator (axum)  │
                  │   - HTTP/gRPC API            │
                  │   - Routes session requests  │
                  │   - Tracks host state        │
                  │   - Owns Postgres metadata   │
                  └───────────────┬──────────────┘
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
         ┌─────────┐         ┌─────────┐         ┌─────────┐
         │ Host A  │         │ Host B  │         │ Host C  │
         │ engram- │         │ engram- │         │ engram- │
         │ host    │         │ host    │         │ host    │
         │ agent   │         │ agent   │         │ agent   │
         │   │     │         │   │     │         │   │     │
         │   ▼     │         │   ▼     │         │   ▼     │
         │microsbx │         │microsbx │         │microsbx │
         │+Firec.  │         │+Firec.  │         │+Firec.  │
         │snap-mgr │         │snap-mgr │         │snap-mgr │
         │pool-warm│         │pool-warm│         │pool-warm│
         └────┬────┘         └────┬────┘         └────┬────┘
              │                   │                   │
              └───────────────────┴───────────────────┘
                                  ▼
                        ┌─────────────────────┐
                        │  BlobStorage        │  ← snapshots (cold tier)
                        │  (GCS / S3 / Min.)  │     image registry
                        └─────────────────────┘

                                  ▼
                        ┌─────────────────────┐
                        │  Postgres           │  ← sessions, messages,
                        │  (managed)          │     snapshot index, etc.
                        └─────────────────────┘
```

### Component summary

- **`engram-coordinator`**: stateless HTTP/gRPC service. Owns scheduling, host registry, session metadata. Backed by Postgres. Multiple replicas behind a load balancer in production; one binary in dev.
- **`engram-host-agent`**: per-host daemon. Drives sandbox lifecycle and snapshot/restore through `SandboxBackend`. Production: `engram-sandbox-firecracker` (Firecracker's HTTP-over-Unix-socket API + UFFD-backed memory restore). Dev: `engram-sandbox-process` (subprocesses, no isolation, runs anywhere including macOS). Manages warm pool, snapshot manager, resource governor, eviction signal handler. Reports state via gRPC heartbeat to the coordinator.
- **`engram-image-builder`**: cron job (or k8s CronJob, or systemd timer). Per repo, every 30 min: clone repo, install deps, warm caches, snapshot the OCI image, push to image registry. Versioned with timestamp tags.
- **`engram-cli`**: ops/admin tool. `engram session list`, `engram host drain`, `engram image build`, etc.

---

## Source-of-truth model

Three layers of state, with explicit durability guarantees:

| Layer | Contents | Durability | On loss |
|---|---|---|---|
| **Postgres** | session metadata, message history, tool calls, snapshot index, host registry | permanent (managed/backups) | hard failure — must guard |
| **Git remote** | committed code changes (agent commits eagerly to a session-scoped branch) | permanent | hard failure — must guard |
| **Image registry** | warm-pool base images | reproducible from Dockerfiles | rebuild |
| **Snapshot store** | microVM memory + FS snapshots | best-effort (hot local + cold GCS) | slow resume, no data loss |

**Design rule**: snapshots are a *cache*, not source of truth. Any snapshot may be stale or absent at any time, and the system MUST work correctly without it. Loss → cold rebuild from image + git + replayed conversation. Slower (~30-90s) but clean.

---

## Components — detailed

### Coordinator (`engram-coordinator`)

**Responsibilities:**
- Receive session requests (`POST /sessions`). Pick a host, return host endpoint + session token.
- Maintain host registry. Heartbeats from each host every 5s: free RAM, warm pool state, snapshots held locally.
- Track snapshot location index (which session's snapshot is on which host vs. only in BlobStorage).
- Drive scheduling decisions: prefer host with snapshot local → host with warm pool for repo → any host with capacity.
- Drive snapshot eviction policy at fleet level (TTL, disk pressure).
- On host failure detected via missed heartbeats: mark sessions on that host as "needs reschedule", fail-over via BlobStorage-backed snapshot.

**Tech:**
- Rust 2021, axum, tokio multi-thread
- sqlx for Postgres (compile-checked queries)
- tonic for gRPC heartbeat channel from hosts
- tracing + tracing-subscriber for observability

**State:**
- All in Postgres. Coordinator instances are stateless and horizontally scalable.

### Host agent (`engram-host-agent`)

**Responsibilities:**
- Maintain warm pool of N microVMs (or pre-warmed snapshots — see "Pool warmer" below) per active repo.
- On checkout: hand a warm VM to a session, replenish pool in background.
- Drive `SandboxBackend` for VM lifecycle (create/exec/snapshot/restore/destroy).
- Run snapshot manager (see below).
- Subscribe to GCE/EC2 preemption signals; on signal, drain uploads and refuse new work.
- Enforce per-VM resource limits via the production backend (Firecracker enforces RAM/CPU/disk at the VMM boundary; the dev backend ignores them with a documented caveat).
- Heartbeat coordinator with capacity + warm pool state.

**Tech:**
- Rust 2021, tokio
- `engram-sandbox-firecracker`: `hyper` over `tokio::net::UnixStream` (via `hyperlocal`) for the Firecracker control API; `userfaultfd(2)` for snapshot restore
- `engram-sandbox-process`: `tokio::process::Command` + per-sandbox cwds; tar+gzip for the dev "snapshot" path
- google-cloud-storage / aws-sdk-s3 / S3-compatible client (BlobStorage trait)

### Snapshot manager (lives inside host agent)

Two-tier storage as discussed:

- **Local NVMe (hot tier)**: most-recently-accessed snapshots, capped at e.g. 500 GB. Sub-second restore.
- **BlobStorage (cold tier)**: every snapshot eventually replicated. ~10-30s restore via download.

**Upload triggers** (priority order):
1. Preemption notice received (drop everything, upload uncommitted snapshots in parallel).
2. Snapshot age > N minutes (typical: 5 min) and not yet replicated.
3. Local disk pressure (> 70% of allocated cap).

**Eviction rules** (from local, all must hold):
- Replicated to BlobStorage.
- Not currently restoring.
- LRU among eligible.

**Compression**: zstd-3 in-flight during BlobStorage upload (memory snapshots compress ~2× well). Local stays uncompressed for fast restore.

**Schema** (per snapshot, in Postgres `snapshots` table): `session_id, host_id, local_path, blob_url, image_version, size_bytes, replicated_at, created_at, last_accessed_at`.

### Pool warmer (lives inside host agent)

- Periodically reconciles `target_warm_count` per repo (configured globally + per-repo overrides).
- When pool count drops (checkout happened), spawn replacement in background.
- On image refresh (new version published): let existing warm VMs drain naturally on checkout; new spawns use new image. Don't migrate in flight.

### Image builder (`engram-image-builder`)

- Cron-driven. Per repo, every 30 min:
  1. Spawn temporary build sandbox.
  2. `git clone <repo>` (using GitHub App token).
  3. Run repo's setup script (e.g., `pnpm install`, `cargo fetch`, type-check).
  4. Snapshot the resulting OCI image.
  5. Push to image registry with tag `<repo>:warm-<timestamp>`.
  6. Mark in Postgres `image_versions` table: `(repo, tag, created_at, status)`.
- Old images GC'd after configurable TTL (default 24h).
- Refresh failures alert via observability stack but don't break warm pool — old image remains until next successful build.

---

## Pluggability — trait design

Four traits define the cloud/storage/sandbox seams. Each ships at least one implementation in v1 plus a stub/mock for testing.

### `CloudBackend` (in `engram-core`, impls in `engram-cloud-*`)

```rust
#[async_trait]
pub trait CloudBackend: Send + Sync {
    /// Subscribe to preemption/eviction notices for the host this is running on.
    /// Returns a stream of PreemptionNotice events (typically a single event).
    fn preemption_signal(&self) -> BoxStream<'static, PreemptionNotice>;

    /// Get host metadata (instance ID, zone, machine type) for self-identification.
    async fn host_metadata(&self) -> Result<HostMetadata, BackendError>;

    /// Optional: provision a new host. Used by future autoscaling.
    /// May be unimplemented (returns NotSupported) for static-fleet deployments.
    async fn provision_host(&self, spec: HostSpec) -> Result<HostId, BackendError>;

    /// Optional: tear down a host.
    async fn deprovision_host(&self, id: HostId) -> Result<(), BackendError>;
}
```

**v1 implementations**:
- `engram-cloud-gcp`: GCE metadata server polling for preemption (ACPI G2 Soft Off + metadata flag), Compute Engine API for provision/deprovision.
- `engram-cloud-static`: no-op preemption signal, returns hostname for metadata, errors on provision (used for Hetzner / bare metal where hosts are static).
- `engram-cloud-mock`: testing only.

**v2+**: `engram-cloud-aws` (Spot eviction via IMDS), `engram-cloud-hetzner` (Cloud API).

### `BlobStorage` (in `engram-core`, impls in `engram-storage-*`)

```rust
#[async_trait]
pub trait BlobStorage: Send + Sync {
    async fn put(&self, key: &str, data: BoxStream<'static, Bytes>) -> Result<(), StorageError>;
    async fn get(&self, key: &str) -> Result<BoxStream<'static, Bytes>, StorageError>;
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError>;
}
```

**v1 implementations**:
- `engram-storage-gcs`: google-cloud-storage crate.
- `engram-storage-s3`: aws-sdk-s3 (also covers MinIO and any S3-compatible API).
- `engram-storage-local`: filesystem-backed for single-host dev.

### `MetadataStore` (in `engram-core`)

Postgres-only in v1; trait exists so SQLite is possible for embedded deployments later.

```rust
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError>;
    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError>;
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError>;
    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError>;
    async fn list_snapshots_for_session(&self, sid: SessionId) -> Result<Vec<SnapshotRecord>, MetaError>;
    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError>;
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError>;
    // ... etc.
}
```

### `SandboxBackend` (in `engram-core`)

The VMM seam. Production = Firecracker on Linux. Dev = host subprocesses on any platform. Future backends (Cloud Hypervisor, raw libkrun, Kata) plug into the same trait if we ever need them.

```rust
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    async fn exec(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecHandle, SandboxError>;
    async fn snapshot(&self, id: SandboxId, dest: &Path) -> Result<SnapshotMetadata, SandboxError>;
    async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;
}
```

**v1 implementations**:

- **`engram-sandbox-firecracker` (production)** — drives Firecracker over its HTTP-over-Unix-socket API. Each sandbox owns a `firecracker-jailer`-managed process, a per-VM TAP device, and a vsock channel to an in-guest agent (see "In-guest agent" below). Snapshot via `PATCH /vm Paused` + `PUT /snapshot/create`. Restore via `PUT /snapshot/load` with `userfaultfd`-backed memory: pages stream in lazily on guest fault, giving sub-100ms resume regardless of guest RAM size. Currently shipped as a typed stub with every method returning a structured error pointing at the Firecracker endpoint to wire — Phase 2 fills it in.
- **`engram-sandbox-process` (dev)** — runs commands as plain host subprocesses, each rooted in a per-sandbox working directory. No isolation, no resource enforcement. Exists so the entire orchestration layer (coordinator API, scheduler, snapshot manager, warm pool, blob storage, host-agent loops) iterates locally on macOS Apple Silicon without any VMM. `snapshot()` is a tarball of the workdir; functional enough to exercise the snapshot manager's LRU/replication paths, not enough to evict in-memory state. **Never use in deployment.**

#### In-guest agent (`engram-agentd`, future)

Firecracker has no "exec a command in a running guest" primitive. Production rootfs images include a small daemon that listens on vsock and proxies exec / stdin / stdout for the host agent. `SandboxBackend::exec` on the Firecracker backend becomes "send the exec request to `engram-agentd` over `(vsock_cid, port=1024)` and stream the response." This is a separate workstream from the Firecracker plumbing itself; AWS Lambda's runtime, Modal's agent, and similar platforms all do this. Full design in the [In-guest agent](#in-guest-agent-engram-agentd) section below.

---

## Communication architecture

This section documents the design for **coordinator ↔ host-agent** and **host ↔ guest** communication. v1 (everything we've built so far) runs both in one process, so this is forward-looking — Phase 3 wires the multi-host channel; Phase 2 wires the host-to-guest channel.

### Coordinator ↔ host-agent: phone-home WebSocket

**Hosts dial the coordinator over a long-lived bidirectional connection**, not the other way around. New hosts only need to know the coordinator URL — there's no inventory file to update, no service discovery to deploy. The connection itself is the discovery: while you're connected, you're alive.

```text
   ┌──────────────┐                             ┌──────────────┐
   │  host-agent  │  wss://coordinator/api/     │ coordinator  │
   │  (any cloud) │  hosts/connect              │  (HA, behind │
   │              │ ──── connect ─────────────► │   load bal.) │
   │              │ ◄─── HostRegistered ─────── │              │
   │              │                             │              │
   │              │ ──── Heartbeat (5s) ──────► │              │
   │              │ ──── CapacityReport ──────► │              │
   │              │ ──── WarmPoolReport ──────► │              │
   │              │ ──── LocalSnapshots ──────► │              │
   │              │                             │              │
   │              │ ◄─── AssignSession ──────── │              │
   │              │ ◄─── RevokeSession ──────── │              │
   │              │ ◄─── Drain ──────────────── │              │
   │              │                             │              │
   │              │ ──── ExecEvent (stdout) ──► │              │
   │              │ ──── SnapshotComplete ────► │              │
   └──────────────┘                             └──────────────┘
```

**Why WebSocket instead of gRPC streaming.** Both work; we choose WS:

- We already use axum, which has first-class WS support — no protobuf toolchain
- Wire types live in `engram-protocol` with serde (validated at parse time)
- Browsers, `wscat`, and curl debug the same channel as production
- gRPC streaming has subtle edge cases (HTTP/2 stream cancellation, half-close, GOAWAY) we don't want to relitigate
- The traffic shape is "events both ways," not strict RPC — WS fits better

**Why hosts dial coordinator (not vice versa).** Production hosts are typically behind NAT (cloud VMs, Hetzner, on-prem). Outbound connections always work; inbound requires firewall punching. Hosts dialing means a fresh box can `ENGRAM_COORDINATOR_URL=wss://... engram-host-agent` and immediately participate.

### Message types

Defined in `engram-protocol` with serde. The discriminant is `type` (tagged enum); each variant carries its own payload.

**Host → coordinator:**

| Type | Payload | Frequency |
|---|---|---|
| `Hello` | `{ host_id, version, capabilities, hostname, cloud_metadata }` | once on connect |
| `Heartbeat` | `{ host_id, sent_at, capacity, draining }` | every 5s |
| `WarmPoolReport` | `{ pools: [{ repo, image_version, ready, target }] }` | on change |
| `LocalSnapshotInventory` | `{ snapshots: [{ id, session_id, size_bytes, replicated }] }` | on change |
| `SessionEvent` | An `IndexedEvent` from a session this host owns — forwarded to coordinator's persistent log + bus | live during exec |
| `Pong` | `{ id }` | response to coordinator-initiated `Ping` |

**Coordinator → host:**

| Type | Payload | When |
|---|---|---|
| `HostRegistered` | `{ host_id, server_time, sessions_already_assigned }` | response to `Hello` |
| `AssignSession` | `{ session_id, repo, image_version, restore_from_blob? }` | new session routed here |
| `RevokeSession` | `{ session_id, upload_snapshot }` | session migrating to another host or terminating |
| `Drain` | `{ deadline_secs }` | preemption or operator drain — refuse new work |
| `Ping` | `{ id }` | health check |

These shapes are already roughly in `engram-protocol::heartbeat` and `engram-protocol::scheduling` — the WS work is wiring them onto a transport, not redesigning them.

### Stream multiplexing

The single WS carries control + per-session events both ways. Each frame is `{ stream: u64, payload: <typed> }` — `stream = 0` is the host control plane (heartbeats, registration); `stream > 0` is per-session traffic. Backpressure is per-stream so a chatty agent can't starve heartbeats.

We start with one WS per host (stream-multiplexed). If backpressure interactions get fiddly under load, we move to one WS per session (one-shot connection per `AssignSession`, closed at session end). Both designs use the same message types — refactor is local.

### Reconnect & state recovery

Disconnect is detected by either side via WS close or missed heartbeat (3 missed = ~15s). The host retries with exponential backoff (initial 1s, max 30s, jitter). On reconnect:

1. Host sends `Hello` with its persisted `host_id`
2. Coordinator looks up sessions still assigned to that `host_id` in Postgres
3. Coordinator replies `HostRegistered { sessions_already_assigned }`
4. Host reconciles: any session it doesn't have locally is presumed lost; it tells the coordinator via `SessionLost { session_id }`
5. Coordinator marks those sessions for restore-from-blob on next access

If the coordinator doesn't see a heartbeat for `dead_timeout` (~30s default), it marks the host `Dead` and reassigns its sessions. A host that reconnects after a `Dead` mark gets the same re-registration flow; sessions that were already reassigned stay where they are.

### Coordinator HA

Multiple coordinator replicas behind a load balancer. Hosts dial *any* replica. The replica that holds the connection owns the in-memory routing for that host. State is consistent because nothing meaningful lives in coordinator memory — host registry, scheduling decisions, assignments, session events — all in Postgres.

On replica failover, hosts reconnect (LB picks a different replica), and the new replica picks them up. The seam is invisible to clients.

For client SSE streams (`GET /sessions/:id/events`), the in-memory bus is per-replica. A client connected to replica A doesn't see live events emitted on replica B. Two paths to reconcile:

- **Pubsub layer for live events**: replicas publish `IndexedEvent`s to Postgres `LISTEN/NOTIFY` (cheap) or Redis pubsub (flexible); each replica subscribes for any session whose SSE clients are connected to it.
- **Sticky sessions**: LB routes a client's `/events` request to whichever replica owns the producing host's WS. Simpler to deploy; tighter coupling between client connection and infra.

We start without either; `LISTEN/NOTIFY` is the no-extra-infra answer when we need it.

### Auth

Host-agents authenticate to the coordinator via one of:

1. **Pre-shared token** (simplest) — `ENGRAM_HOST_TOKEN=...` on the host; coordinator validates. Fine for single-org deployments
2. **mTLS** — host has a per-host cert signed by an internal CA; coordinator validates cert chain. Better for multi-tenant
3. **Cloud workload identity** — host presents a signed JWT from GCE / IMDSv2 / etc.; coordinator validates with the cloud's JWKS. Best for cloud deployments

Phase 4 adds (1) and the trait surface for the others.

---

## In-guest agent (`engram-agentd`)

Phase 2 dependency. A small Rust binary baked into every production rootfs, started by init at guest boot. Bridges the gap between Firecracker (which has no exec primitive) and the host agent.

```text
            host-agent                            ┌── guest VM ──────────┐
                │                                 │  PID 1: init         │
                │   PUT /vsock { guest_cid: N }   │   ↓                  │
                ├────────────────────────────────►│  engram-agentd       │
                │                                 │  vsock listen :1024  │
                │   vsock connect (cid=N, port=1024)                     │
                ├────────────────────────────────►│                      │
                │   {token frame}                 │  ← validate          │
                │   Exec { argv, env, cwd }       │  ← spawn child       │
                │ ◄──── Stdout(...) ─────────────┤                      │
                │ ◄──── Stdout(...) ─────────────┤                      │
                │ ◄──── Exit { status: 0 } ──────┤                      │
                └─────────────────────────────────└──────────────────────┘
```

### Transport — virtio-vsock

Why vsock and not e.g. SSH-over-virtio-net or a TCP server in the guest:

- Works the moment the kernel boots — no IP, no DHCP, no TAP setup
- No accidental routing surface (vsock is host↔guest only by design)
- Both KVM and Apple Hypervisor.framework support virtio-vsock
- Linux kernel >= 5.6 ships the `vhost-vsock` driver out of the box
- The `(host_cid, guest_cid, port)` triple is a clean addressing model

We reserve port `1024` (declared as `ENGRAM_AGENTD_PORT` in `engram-sandbox-firecracker`) for agentd.

### Authentication

Per-session token, validated on the first frame of each new connection. Never travels in env vars (which are visible to any in-guest process) and never appears on disk.

Three injection options, in order of preference:

1. **First-frame handshake (chosen)** — host opens vsock connection, sends `{ token }` as the first frame; agentd validates. Token never persists anywhere on the guest; rotates per connection. Robust against any in-guest exfiltration short of a kernel exploit.
2. **virtio-fs single-file mount** — `/run/engram/token` mode 0400 owned by root. Visible to root processes inside the guest; mount point goes away on shutdown.
3. **Kernel cmdline** — `engram_token=...` injected via `BootSource.boot_args`. Visible in `/proc/cmdline` to any in-guest reader. Simplest; least secure.

We pick (1).

### Protocol

Length-prefixed CBOR (compact, schema-stable across language bindings). Each frame is `[u32 length][cbor payload]`. Top-level message is a tagged enum mirroring `engram-protocol`'s exec types.

```rust
// crates/engram-protocol/src/agentd.rs (Phase 2)

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentRequest {
    Auth { token: String },
    Exec {
        request_id: u64,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout_secs: Option<u64>,
    },
    Stat { request_id: u64, path: String },
    Upload { request_id: u64, path: String, contents: Vec<u8>, mode: u32 },
    Download { request_id: u64, path: String },
    Ping { request_id: u64 },
    Shutdown { graceful: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentResponse {
    AuthOk,
    AuthFailed { reason: String },
    Stdout { request_id: u64, chunk: Vec<u8> },
    Stderr { request_id: u64, chunk: Vec<u8> },
    Exit { request_id: u64, status: Option<i32> },
    StatResult { request_id: u64, /* size, mode, mtime */ },
    UploadOk { request_id: u64 },
    DownloadChunk { request_id: u64, chunk: Vec<u8> },
    DownloadDone { request_id: u64 },
    Pong { request_id: u64, agent_info: AgentInfo },
    Error { request_id: u64, message: String },
}
```

Multiple in-flight requests are demuxed by `request_id`, so the host agent can pipeline `Exec` calls without waiting for the previous one to complete.

### Lifecycle

1. **Init** — guest's init system (we ship one of: minimal busybox-init, OpenRC, systemd) starts agentd as a service. agentd listens on `vsock://(any, 1024)`.
2. **Host connects** — sandbox backend opens vsock, sends `Auth { token }`. agentd replies `AuthOk` or `AuthFailed`.
3. **Steady state** — host pipelines `Exec` and other requests; agentd spawns subprocesses for `Exec`, streams stdout/stderr back as it produces output.
4. **Shutdown** — host sends `Shutdown { graceful }` before snapshot or destroy. agentd kills any in-flight subprocesses, flushes pending output, closes the connection cleanly.

### Distribution

agentd binary is baked into every production rootfs at `/usr/local/bin/engram-agentd`, statically linked (musl target) so it runs in any base image without libc dependencies. The image-baker pipeline (`engram-image-builder`) injects it during rootfs construction.

Versioning: agentd sends its version string in the `Pong.agent_info` response. Coordinator can refuse to run on hosts whose images carry an agentd that's too old to speak the current protocol — surfaces incompatibility cleanly rather than as mysterious RPC errors.

---

## Database schema (Postgres)

Bare-bones v1 schema. All tables get `id UUID PRIMARY KEY DEFAULT gen_random_uuid()`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ`.

```sql
CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    user_id TEXT,
    status TEXT NOT NULL,                    -- pending | active | idle | completed | failed
    image_version TEXT NOT NULL,             -- the warm image this session was created against
    host_id UUID REFERENCES hosts(id),       -- current host (NULL if evicted-only-in-blob)
    created_at TIMESTAMPTZ NOT NULL,
    last_active_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE hosts (
    id UUID PRIMARY KEY,
    hostname TEXT NOT NULL UNIQUE,
    cloud_metadata JSONB,                    -- instance ID, zone, machine type
    capacity_total_gb INT NOT NULL,
    capacity_used_gb INT NOT NULL,
    last_heartbeat_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL                     -- ready | draining | dead
);

CREATE TABLE snapshots (
    id UUID PRIMARY KEY,
    session_id UUID REFERENCES sessions(id) ON DELETE CASCADE,
    host_id UUID REFERENCES hosts(id),       -- NULL if local copy gone
    local_path TEXT,                         -- NULL if not on disk
    blob_url TEXT,                           -- NULL until upload completes
    image_version TEXT NOT NULL,             -- warm image at time of snapshot
    size_bytes BIGINT NOT NULL,
    replicated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL,
    last_accessed_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE messages (
    id UUID PRIMARY KEY,
    session_id UUID REFERENCES sessions(id) ON DELETE CASCADE,
    idx INT NOT NULL,                        -- ordering within session
    role TEXT NOT NULL,                      -- user | assistant | tool
    content_json JSONB NOT NULL,
    tokens INT,
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE (session_id, idx)
);

CREATE TABLE tool_calls (
    id UUID PRIMARY KEY,
    session_id UUID REFERENCES sessions(id) ON DELETE CASCADE,
    message_id UUID REFERENCES messages(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    input_json JSONB NOT NULL,
    output_json JSONB,
    exit_status INT,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE agent_commits (
    id UUID PRIMARY KEY,
    session_id UUID REFERENCES sessions(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    branch TEXT NOT NULL,
    pushed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE image_versions (
    id UUID PRIMARY KEY,
    repo TEXT NOT NULL,
    tag TEXT NOT NULL,                       -- warm-<timestamp>
    blob_url TEXT,                           -- where image lives
    status TEXT NOT NULL,                    -- building | ready | retired
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE (repo, tag)
);

CREATE INDEX idx_snapshots_session ON snapshots(session_id);
CREATE INDEX idx_snapshots_host ON snapshots(host_id);
CREATE INDEX idx_messages_session_idx ON messages(session_id, idx);
CREATE INDEX idx_sessions_status ON sessions(status) WHERE status IN ('pending', 'active', 'idle');
```

---

## Repository layout (Rust workspace)

Mirroring claw-code's pattern of `crates/*` with internal path dependencies. No `thiserror`/`anyhow` — custom enum-based error types (see claw-code for examples). `serde`, `tokio`, `sqlx`, `axum`, `tracing` shared via `[workspace.dependencies]`.

```
engram/
├── Cargo.toml                         # workspace root
├── README.md
├── DESIGN.md                          # this doc, edited as design evolves
├── LICENSE                            # Apache 2.0
├── .github/
│   └── workflows/                     # CI: fmt, clippy, test, build
├── docker/
│   ├── coordinator.Dockerfile
│   └── host-agent.Dockerfile
├── deploy/
│   ├── gcp/                           # Terraform for reference GCP deployment
│   ├── docker-compose.yml             # single-host dev
│   └── migrations/                    # sqlx migrations
└── crates/
    ├── engram-core/                   # types, traits, errors. No I/O.
    │   └── src/
    │       ├── traits/                # CloudBackend, BlobStorage, MetadataStore, SandboxBackend
    │       ├── types/                 # SessionId, HostId, SnapshotMetadata, etc.
    │       └── error.rs
    ├── engram-coordinator/            # binary: HTTP/gRPC service
    ├── engram-host-agent/             # binary: per-host daemon
    ├── engram-image-builder/          # binary: cron-style image baker
    ├── engram-cli/                    # binary: ops/admin
    ├── engram-sandbox-firecracker/    # impl of SandboxBackend driving Firecracker (production)
    ├── engram-sandbox-process/        # impl of SandboxBackend using host subprocesses (dev only)
    ├── engram-cloud-gcp/              # impl of CloudBackend for GCE
    ├── engram-cloud-static/           # impl of CloudBackend for bare-metal/Hetzner
    ├── engram-cloud-mock/             # impl of CloudBackend for tests
    ├── engram-storage-gcs/            # impl of BlobStorage for GCS
    ├── engram-storage-s3/             # impl of BlobStorage for S3 / MinIO
    ├── engram-storage-local/          # impl of BlobStorage for local FS
    ├── engram-postgres/               # impl of MetadataStore for Postgres
    └── engram-protocol/               # gRPC/protobuf definitions for coordinator <-> host
```

**Key dependency choices** (informed by exploration of claw-code patterns + 2026 Rust ecosystem):

- async runtime: `tokio` 1.x with `rt-multi-thread`
- HTTP server: `axum` 0.7
- gRPC: `tonic` (heartbeats, streaming exec)
- Postgres: `sqlx` with compile-checked queries
- HTTP client: `reqwest` 0.12 with rustls-tls
- GCS: `google-cloud-storage` from googleapis/google-cloud-rust
- AWS/S3: `aws-sdk-s3`
- Observability: `tracing` + `tracing-subscriber`
- Errors: custom enums per crate (claw-code pattern), no `thiserror`/`anyhow`
- Edition 2021, `forbid(unsafe_code)` workspace-wide

---

## Phased implementation plan

Order is deliberate: each phase produces something runnable end-to-end. Don't build pluggability before there's something to plug into.

### Phase 1 — Orchestration layer end-to-end on the dev backend (largely done)

**Goal**: one binary, one host, one repo, end-to-end session, runnable on macOS Apple Silicon.

- ✅ `engram-coordinator` HTTP API: `POST /sessions`, `GET/DELETE /sessions/:id`, `POST /sessions/:id/exec`, plus snapshot/resume/evict-local stubs.
- ✅ `engram-sandbox-process` real implementation: per-sandbox cwd, real subprocess exec, tarball-based snapshot/restore.
- ✅ `engram-cloud-{static,gcp,mock}`, `engram-storage-{local,gcs-stub,s3-stub}`, `engram-postgres`.
- ✅ SandboxRegistry + lifecycle wiring: create allocates a sandbox, exec routes through it, delete tears it down.
- ✅ Warm-pool data structure (no SandboxBackend-driven replenish yet).
- ✅ Postgres schema + migration runner.
- ✅ ~120 tests including end-to-end API integration via `tower::ServiceExt::oneshot`.

**Deliverable**: `just dev` brings up Postgres + coordinator (subprocess backend) on a Mac. `curl POST /sessions` returns a session, `POST /sessions/:id/exec` round-trips real stdout/stderr/exit-status.

### Phase 2 — Firecracker integration (Linux production path)

**Goal**: production-grade isolation, snapshot-evict mechanic working end-to-end.

Concrete deliverables:

- **`engram-sandbox-firecracker` real implementation**
  - `hyper`-over-`tokio::net::UnixStream` (via `hyperlocal`) client to the Firecracker control socket
  - Per-sandbox jailer setup: `firecracker-jailer` invocation that drops privileges, chroots, applies cgroups, sets up seccomp filters, passes `/dev/kvm` fd
  - VM config: `PUT /machine-config` + `/boot-source` + `/drives/rootfs` + `/network-interfaces/eth0` + `/vsock`, then `PUT /actions InstanceStart`
  - TAP-per-VM networking, host-side bridge or routed network, predictable IP allocation
  - Lifecycle: `SendCtrlAltDel` for graceful, SIGKILL for forced
- **`engram-agentd`** — full design in [In-guest agent](#in-guest-agent-engram-agentd) section above. Phase 2 makes it real: musl-static binary, virtio-vsock protocol, first-frame token auth, exec/stat/upload/download/ping/shutdown methods.
- **Snapshot/restore via Firecracker API**
  - Take: `PATCH /vm Paused` → `PUT /snapshot/create { snapshot_path, mem_file_path, snapshot_type: "Full" }` → `PATCH /vm Resumed`
  - Restore: `PUT /snapshot/load { snapshot_path, mem_backend: { backend_type: "Uffd", backend_path: <handler socket> }, resume_vm: true }`
  - **UFFD memory backend** (the load-bearing part): a host-agent task accepts page-fault events on the UFFD socket and pages in 4 KiB chunks from the on-disk memory file. Resume returns immediately; pages stream in lazily on guest fault. Sub-100ms warm time regardless of guest RAM.
- **Snapshot manager (real)** — host-agent's `SnapshotManager::replicate` TODO becomes: stream local NVMe → BlobStorage with `async-compression` zstd-3, mark `replicated_at`. LRU eviction once replicated.
- **Real `engram-storage-{gcs,s3}`** — fill the typed-stub crates with the SDK calls. Match the trait shape we already have.
- **Broker-mode secret proxy** — per-session HTTPS-MITM proxy that substitutes placeholders for real values on outbound requests matching `schema.allow_hosts`. TLS interception requires generating a per-session CA cert + injecting it into the guest trust store at first boot. Network namespace setup makes the proxy the only egress path so the agent can't bypass.
- **Network policy enforcement** — iptables/nftables rules at the TAP boundary derived from `manifest.network`. Default-deny outbound; allowlist hosts get DNS + connection through the proxy.

**Deliverable**: on a Linux box with KVM, idle a session, evict from RAM via `DELETE /sessions/:id/local`, resume in <100ms via UFFD-backed restore. Outbound HTTPS to `api.github.com` succeeds with a real token; outbound HTTPS to `evil.com` is blocked at the network layer; the agent inside the sandbox never sees the real `GITHUB_TOKEN` value.

### Phase 3 — Multi-host coordinator

**Goal**: two hosts working together; new hosts come online with zero coordinator-side config.

- **Split binaries**: `engram-host-agent` runs separately, dials coordinator over WebSocket. Coordinator binary's `--mode=all` keeps the in-process embedding working for single-host dev.
- **WebSocket phone-home transport** — full design in [Communication architecture](#communication-architecture) above. Concretely:
  - `wss://coordinator/api/hosts/connect` axum handler
  - `engram-host-agent` outbound dialer with exp-backoff reconnect
  - Stream-multiplexed frames carrying `engram-protocol` message types
  - On reconnect: host sends `Hello { host_id }`, coordinator replies with `sessions_already_assigned`, host reconciles
- **Host registry** — Postgres-backed (existing schema), updated from heartbeats; coordinator marks `Dead` after missed heartbeats and reassigns.
- **Multi-host scheduler** — replaces the trivial single-host picker:
  - Prefer host with the snapshot held locally (zero-cost hot-tier hit)
  - Fall back to host with a warm pool for this `(repo, image_version)`
  - Fall back to host with capacity, no warm pool
  - Returns `BackendError::NotSupported("no host available")` if everything's full
- **Session migration**: when a host goes dark, mark its sessions `pending-reassign`. Next access pulls the latest snapshot from BlobStorage and starts the session on a different host.
- **`POST /sessions/:id/migrate { host_id }`** — operator-initiated migration for evacuating a host before maintenance.
- **Coordinator HA** — multiple replicas behind a load balancer; `LISTEN/NOTIFY` on Postgres for cross-replica live event propagation when an SSE client is connected to a different replica than the producing host.

**Deliverable**: stand up coordinator (2 replicas) + 3 hosts; sessions distribute. Kill one host with `kill -9 firecracker-pid` and observe sessions migrate within 30s. Restart a coordinator replica; client SSE streams transparently survive.

### Phase 4 — Cloud abstraction & spot tolerance

**Goal**: GCP-native production deployment; gracefully survive Spot/preemptible eviction; clean trait surfaces for non-GCP adopters.

- **`engram-cloud-gcp`** real implementation
  - GCE metadata server polling for `instance/maintenance-event` and `instance/preempted`
  - Compute Engine API (via `google-cloud-compute`) for autoscaling primitives — `provision_host` / `deprovision_host` flip from `NotSupported` stubs to real
- **Preemption drain handler** — on signal:
  1. Coordinator marks host `draining`, refuses new assignments
  2. Host force-snapshots all in-RAM sessions, fan-out uploads to BlobStorage in parallel
  3. Host upgrades unreplicated → replicated as fast as possible before deadline
  4. After deadline (or upload completion), session migrations land on remaining hosts; new sessions start on demand restoring from blob
- **Cloud autoscaler** (optional, runs in coordinator) — provisions hosts when fleet pool pressure exceeds a threshold; deprovisions on slack
- **Reference deployments**:
  - `deploy/gcp/` Terraform module — managed instance group of preemptible n2d-highmem hosts with nested-virt, plus a coordinator service running on Cloud Run or GKE
  - `deploy/hetzner/` setup script — bare-metal AX-series box with KVM, coordinator on the same host (no autoscaling)
  - `deploy/bare-metal/` — single-host, no cloud SDK, suitable for self-hosters
- **`engram-cloud-aws`** + **`engram-cloud-hetzner`** — typed stubs that fill in as adopters need them

**Deliverable**: stand up the `deploy/gcp/` Terraform on a real GCP project. Run a load test that triggers preemption via `gcloud compute instances simulate-maintenance-event`. Sessions seamlessly resume on remaining hosts; no client-visible downtime.

### Phase 5 — Image baking pipeline

**Goal**: warm-pool images refresh on a schedule, no human in the loop.

- **`engram-image-builder` real implementation** (the existing stub):
  - Cron / k8s CronJob / systemd timer triggers per-repo every ~30 min
  - Spawn temporary build sandbox (using whichever sandbox backend is configured)
  - `git clone <repo>` (using GitHub App token from SecretStore, with fine-grained `contents:read` scope for that repo)
  - Run repo's setup script declared in `engram.toml` (the per-repo config — `setup = ["pnpm install", "cargo fetch"]` etc.)
  - For Firecracker: snapshot the resulting filesystem to `rootfs.ext4` via `mkfs.ext4` + `tar` extraction; for ProcessBackend: tarball the workdir into a `rootfs/` directory
  - Inject the static `engram-agentd` musl binary into `/usr/local/bin/engram-agentd` and an init unit/service file
  - Write `manifest.toml` (env vars + secret schema + network policy + resources) — this is what production agent code declares its dependencies on
  - Push artifacts to image registry; record `(repo, tag, status=ready)` in `image_versions` table
- **Image registry backend** — for Firecracker production, images live in BlobStorage (GCS/S3) and host-agents fetch on demand. Registry resolution lookups become an HTTP-y thing rather than a filesystem walk
- **Warm pool refresh** — when a new image_version is published, existing warm VMs drain naturally on checkout; new replenishes spawn from the new image. No mid-flight migration
- **Image GC** — old image versions retire after a configurable TTL (default 24h after last access); blobs lifecycle to coldline storage; eventually deleted

**Deliverable**: schedule the baker every 30 min in CI. Observe new image versions appear in `image_versions` table with `status=ready`, and warm-pool VMs spawn from the latest tag without disrupting in-flight sessions.

### Phase 6 — Production operability

**Goal**: cross-cutting concerns that make the system runnable. Some pieces land earlier as adjacent phases need them.

- **Auth**
  - Client tokens (`Authorization: Bearer ...`) with scopes per session/repo
  - Host-agent: pre-shared token (simple), mTLS (multi-tenant), or cloud workload identity (JWT from IMDSv2)
  - Per-session signed URLs for `/events` SSE so a Slack thread URL can't be replayed off-thread
- **Resource enforcement & observation**
  - Linux dev: cgroups v2 hard limits on ProcessBackend (memory.max, cpu.max, io.max) when running as root or with user namespaces
  - Firecracker: native at the VM boundary
  - Per-`exec` resource accounting via `getrusage(RUSAGE_CHILDREN)` — peak RSS / user CPU / sys CPU / wall — surfaced on `ExecCompleted` events
- **Observability**
  - `/metrics` Prometheus exposition: session counts, exec latencies, snapshot sizes, replication lag, pool ready/target ratios
  - Structured tracing (`tracing` + `tracing-subscriber`) with request ids propagated through the WS channel into host-agent logs
  - Audit log derived from `session_events` (filterable by user, repo, event kind)
- **Per-session secret overrides** — `POST /sessions { secrets: { GITHUB_TOKEN: "ref://user/123/github" } }` so a session run on behalf of a Slack user uses their token, not the org's. Refs route through the configured SecretStore exactly like manifest-declared refs
- **Capability surfacing** — `SandboxBackend::capabilities() -> Capabilities { isolated, snapshot, resource_enforcement, streaming }` so the coordinator can include in `/healthz` and refuse certain operations on insufficient backends
- **Rate limiting / quotas** per (repo, user, agent) using Redis or in-memory token buckets
- **TLS** for the WS channel, the HTTP API, and outbound calls to BlobStorage/SecretStore
- **API versioning** — `/v1/` prefix, deprecation policy
- **Postgres operations** — backup strategy, point-in-time recovery, schema-migration discipline (forward-compatible with running coordinators)
- **Snapshot/event GC** — TTLs, partitioning of `session_events` by week for cheap drop-old-partitions

**Deliverable**: a security-and-ops-conscious deployer can run Engram in production. SOC2-style audit log; metrics dashboard; `gcloud iam` integration for host-agent auth; documented rotation procedures for tokens.

### Phase 7 — Clients & adopter experience

**Goal**: the layer that makes Engram useful to people who aren't Cortex.

- **Web app**
  - Real-time view of running sessions per repo / user
  - `EventSource('/events')` rendering exec output with terminal styling
  - File explorer / drag-and-drop upload via `engram-agentd`'s upload/download RPCs
  - Snapshot management: visualise hot vs cold tier, manually trigger snapshot/evict/resume
  - Multi-tab collaborative viewing of one session
- **Slack integration**
  - Bot listens for `/engram <repo>` slash commands or @-mentions
  - Creates a session, posts a thread, subscribes to `/events?since=N`
  - Batches stdout chunks into Slack thread messages on a debounce
  - Reconnects across bot restarts via the persistent event log + `Last-Event-ID`
  - Session-per-thread mapping in a `session_external_ids (provider, external_id, session_id)` table
- **`engram-cli` flesh-out**
  - `engram session list` — running sessions with status, host, last activity
  - `engram session logs <id>` — tail / replay events
  - `engram host drain <id>` — operator drain
  - `engram image build <repo>` — trigger a baker run
  - `engram secret set <name> --ref ...` — write to the configured SecretStore
- **Documentation site** — mdbook with the manifest reference, deployment guides, secret-store backend reference, agent integration examples
- **Reference deployments** — Helm chart (for adopters who want K8s control plane), Terraform modules, single-host docker-compose
- **Example agent integrations** — claw-code, OpenHands, Aider — show how each consumes the API

**Deliverable**: a third-party can stand up Engram and integrate their agent without reading the source code.

### Cross-cutting concerns

These don't fit a single phase but need to be tracked:

- **Test infrastructure**: chaos tests (kill coordinator, kill hosts, kill Postgres briefly), load tests with k6/locust, end-to-end integration tests in CI against a real Firecracker on a Linux runner
- **Documentation discipline** — DESIGN.md stays current; ADRs (Architecture Decision Records) for non-obvious choices; per-crate `lib.rs` docstrings
- **Security review** — at least one external pass on the secret-broker design before production rollout. Threat model: prompt-injected agent tries to exfiltrate secrets, observe other sessions, escape the sandbox
- **Compliance** — SOC2 / HIPAA path if Cortex needs it
- **Developer ergonomics** — keep the `just dev` story working through every phase. If a phase makes Mac dev worse, that's a regression to fix

---

## Open questions / risks

### ~~Risk 1 — VMM snapshot/restore (closed)~~

**Closed.** Production uses Firecracker. Snapshot/restore is a solved problem there: `PATCH /vm Paused` + `PUT /snapshot/create` produces a state file + memory file; `PUT /snapshot/load` with a `userfaultfd`-backed memory backend gives sub-100ms resume regardless of guest RAM size. Both x86_64 and aarch64. Used in production by AWS Lambda, Modal, E2B, Ramp, Fireworks, etc. — i.e. the exact peer set we're modelling.

Historical note: an earlier draft of this design routed sandbox lifecycle through microsandbox (which embeds libkrun) and tried to drop to Firecracker's HTTP socket for snapshot/restore. That doesn't work — microsandbox embeds libkrun **in-process**, there is no out-of-process VMM and no management socket to drive on the side. libkrun's public C API also has no snapshot/save/restore primitives today. We pivoted to "Firecracker for production, subprocesses for dev" rather than commit to a libkrun snapshot upstream campaign. The dual-backend design preserves macOS dev productivity without compromising the production snapshot story.

### Risk 1 — Firecracker integration scope (was Risk 2)

`engram-sandbox-firecracker` is non-trivial: jailer integration, TAP networking, vsock + in-guest agent (`engram-agentd`), UFFD memory backend handler, OCI-image-to-rootfs.ext4 baking. Each piece is well-understood (every microVM-based platform has solved them) but the total surface is meaningful. Mitigation: keep the SandboxBackend trait stable so the dev backend continues to work end-to-end while Firecracker is filled in incrementally; split the work into discrete commits behind the trait (lifecycle, then exec via vsock, then snapshot, then UFFD, then jailer hardening).

### Risk 2 — host virtualization requirements

Firecracker requires KVM. On GCE that means specific machine types (N2, N2D, C3) with `--enable-nested-virtualization` and a 5–15% overhead. Verify our target machine type early (likely n2d-highmem-32 with Spot pricing). Bare metal (Hetzner AX-series) avoids the nested-virt tax entirely and is cheaper per RAM-GB; worth considering for steady-state capacity. macOS contributors don't run Firecracker locally — they use the dev backend (`engram-sandbox-process`) for the orchestration loop and validate against Firecracker on a remote Linux dev box (Hetzner ~$5/mo) or in CI.

### Risk 3 — snapshot size at 16+ GB sessions

A session running backend + frontend + postgres can produce 8–12 GB compressed snapshots. At thousands of sessions/day, this is significant disk + network. Mitigation: aggressive lazy-startup pattern (don't run all services in every session), zstd compression, GCS lifecycle policies for cold tier, Firecracker diff snapshots so subsequent saves only capture dirtied pages.

### Risk 4 — In-guest agent (`engram-agentd`) is its own workstream

Firecracker doesn't expose an "exec a command in a running guest" primitive — exec routes through a small daemon we ship inside the rootfs, listening on vsock. This is a real new component to design and bake into images: protocol (likely length-prefixed CBOR or simple framed JSON over vsock), auth (per-session token injected via kernel cmdline or virtio-fs file), lifecycle (started by init in the guest). Well-trodden — every Firecracker-based platform has built one — but it's not free. Sequence: get Firecracker boot working first, then layer agentd onto the image-baker pipeline.

---

## Starter images & secrets

An "image" is a deployable bundle: rootfs + manifest + (deployment-side) secret keyring. Manifests are publishable; **no secret values ever live in the manifest** — only schema (which secret names the image expects, which hosts they may be substituted on).

### Source-side: `Dockerfile` + `engram.toml`

Devs declare an image with two files in their repo:

```
my-repo/
├── Dockerfile                # WHAT'S in the image — universal Docker syntax
├── engram.toml               # HOW the image is USED — engram-specific config
└── ...source...
```

`Dockerfile` is plain. No engram-specific syntax. Multi-stage builds, `FROM ... AS builder`, `COPY --from=builder ...`, BuildKit cache mounts — all supported, because we just shell out to `docker build`. Devs reuse everything they already know about Dockerfiles.

`engram.toml` carries the runtime manifest fields plus a source-only `[build]` section the runtime doesn't need:

```toml
name = "cortex-api"
description = "Backend API service"
secret_mode = "broker"

[env]
NODE_ENV = "production"

[secrets.GITHUB_TOKEN]
allow_hosts = ["api.github.com"]
required = true

[network]
default = "deny"
allow_hosts = ["api.github.com", "registry.npmjs.org"]

[resources]
suggested_memory_mib = 4096

# Source-only — the baker reads this; runtime doesn't see it.
[build]
dockerfile = "Dockerfile"          # optional, default
context = "."                      # optional, default
args = { BUILD_FLAVOR = "release" }
build_secrets = ["NPM_TOKEN"]      # exposed via BuildKit --secret
```

The split is intentional: `engram.toml` carries *what every backend reads at runtime* (env, secrets schema, network, resources) plus *only what the baker needs* (`[build]`). Production deployments distribute the rendered `manifest.toml` (without `[build]`); the source `engram.toml` stays in the repo.

### Why Dockerfiles, not a custom format

- **It's the universal "what's installed where" language.** Reinventing it would be ecosystem hostility.
- **Multi-stage builds, base-image reuse, build caching, secret mounts** are already solved by BuildKit. Free for us.
- **The OCI image is portable** — same artifact runs as a container too (great for CI, debugging) and converts to `rootfs.ext4` for Firecracker production.
- **Convergence with the field** — Modal, AWS App Runner, Cloud Run, Fly Machines all work this way. There's a reason: container images are the unit of deployable code in 2026.

A small set of Dockerfile concepts don't translate cleanly to a microVM rootfs and the baker documents the mapping:

| Dockerfile directive | In Engram |
|---|---|
| `FROM`, `RUN`, `COPY`, `ADD`, `ENV` | Honored as-is — they shape the rootfs. |
| `CMD`, `ENTRYPOINT` | **Ignored.** The kernel boots, init starts `engram-agentd`, agentd waits for exec RPCs. The user's `CMD` would be a process they start via exec, not the entrypoint. |
| `EXPOSE` | Informational only; networking is at the TAP boundary, not container ports. |
| `USER` | Honored. The agent runs as the image's `USER` if set. |
| `WORKDIR` | Honored as the default exec workdir. |
| `HEALTHCHECK` | Ignored; health is reported by `engram-agentd`. |

### Baker (`engram-image-builder`)

`engram image build --repo <repo> --source <path>` (or directly: `engram-image-builder build --repo ... --source ...`) does:

1. **Read** `<source>/engram.toml` — split into manifest fields + `[build]`
2. **`docker build`** with `-f <build.dockerfile> -t engram-bake-<uuid> --build-arg KEY=VAL ... <build.context>`
3. **`docker create engram-bake-<uuid>`** → throwaway container id
4. **`docker export <container> | tar -x -C <images_dir>/<repo>/<tag>/rootfs/`** — extracts the image's filesystem to the registry path the coordinator's `ImageRegistry` reads from
5. **Write** the rendered `manifest.toml` (engram.toml minus `[build]`) into the same image directory
6. **Cleanup** — `docker rm -f <container>` + `docker rmi -f <build-tag>`. Runs unconditionally so failed bakes don't leak Docker state
7. **Optional**: upsert the `image_versions` row marking this `(repo, tag)` as `Ready`. Skipped when `DATABASE_URL` isn't set (filesystem-only bake — useful for `engram-cli image build` against a coordinator-less workspace)

For Phase 2 / Firecracker production, the same flow extends with two additional steps between (4) and (5):

- **Inject `engram-agentd`** — copy the static musl binary to `/usr/local/bin/engram-agentd` plus an init service unit appropriate to the rootfs (busybox-init / OpenRC / systemd)
- **Convert** `rootfs/` → `rootfs.ext4` via `mkfs.ext4 -d <rootfs>` so Firecracker can attach it as the root drive
- **Inject** the per-deployment broker CA cert into the rootfs's trust store so HTTPS interception works without warnings

These extensions live in the same `engram-image-builder` crate, behind a `--for=firecracker` flag that switches the post-export pipeline. The `--for=process` (default) flow is what we ship now.

### Docker dependency

The baker requires a Docker-compatible runtime on the host. Verified compatible:

- **Docker Desktop** (Mac, Linux, Windows)
- **OrbStack** (Mac — fastest on Apple Silicon)
- **Colima** (Mac — VM-backed)
- **Podman** with `podman-docker` (Linux — daemonless option)
- **BuildKit standalone** via `buildctl` (production CI; we use the `docker` CLI shim)

Override which binary the baker runs with `--docker-bin <name>` or `ENGRAM_DOCKER_BIN`.



### Manifest (`<image_root>/manifest.toml`)

```toml
name = "cortex-api"
description = "Backend API service"
secret_mode = "broker"   # "literal" (dev only) | "broker" (prod)

[env]
PYTHONUNBUFFERED = "1"

[secrets.GITHUB_TOKEN]
allow_hosts = ["api.github.com"]
allow_host_patterns = ["*.githubusercontent.com"]
required = true
description = "Read+write to org cortex/"
# Optional deployment-specific ref. Backends parse the URL.
# Omit to let the configured SecretStore default-namespace by image.
ref = "gcp-sm://projects/cortex-prod/secrets/github-token/versions/latest"

[secrets.OPENAI_API_KEY]
allow_hosts = ["api.openai.com"]
required = false

[network]
default = "deny"
allow_hosts = ["api.github.com", "registry.npmjs.org", "pypi.org"]

[resources]
suggested_memory_mib = 4096
suggested_vcpus = 2
suggested_disk_gib = 20
```

Image directory layout:

```
<image_root>/
  <repo>/                       # e.g. cortex/api → cortex/api/
    <tag>/
      manifest.toml             # required
      rootfs/                   # ProcessBackend (dev): copied into cwd
      rootfs.ext4               # FirecrackerBackend (prod): attached as root
```

Both `rootfs/` and `rootfs.ext4` may coexist — the registry picks based on which the configured backend wants. ProcessBackend on macOS uses APFS `clonefile` (`cp -c -R`) for instant CoW materialization; non-APFS systems fall back to recursive copy.

### `SecretStore` — hot-swappable backend

The image manifest never carries secret values. At session-create time the coordinator calls `SecretStore::resolve(ctx, manifest.secrets)` on the configured backend, which returns a `SecretBundle` of `name → (value, schema)`. Required-but-missing secrets fail the request.

| Backend crate | When |
|---|---|
| `engram-secrets-dev::InMemorySecretStore` | tests |
| `engram-secrets-dev::EnvSecretStore` | local dev — reads `$NAME` from the host shell |
| `engram-secrets-dev::DotenvSecretStore` | local dev — reads from a `.env` file |
| `engram-secrets-gcp::GcpSecretManager` | production on GCP (currently a typed stub; resolves `gcp-sm://...` refs and namespaces by `<repo>--<name>`) |
| (future) `engram-secrets-vault`, `engram-secrets-aws`, `engram-secrets-k8s` | other production deployments |

Switching backends is a coordinator config flag — no manifest changes. The `ref` field on each `SecretSchema` is opaque to the manifest layer; backends parse their own URL format.

### Secret modes

`manifest.secret_mode` picks how resolved values reach the guest:

- **`literal`** — real values land as plain env vars. Simple, fast, dev only. Trivial to set up; secrets are visible to anyone with code execution inside the sandbox.
- **`broker`** — random placeholders (`engram_ph_<sessionId>_<hash>`) land as env vars; a per-session network proxy substitutes the real value only on outbound HTTPS requests whose host matches `schema.allow_hosts` / `schema.allow_host_patterns`. The agent process never sees the real credential, so prompt-injection exfiltration attacks fail. Modeled on microsandbox's `Secret.env(..., allow_hosts=...)` design.

The proxy that performs broker-mode substitution is **not yet implemented** — `secret_mode = "broker"` results in unsubstituted placeholders today. Production rollout for Cortex is gated on this. Implementation lands with the Firecracker network-namespace work in Phase 2.

### Resolution flow

```
POST /sessions
  └─ ImageRegistry::load(repo, tag) → ResolvedImage { manifest, rootfs, ... }
     │  (NotFound → fall through to "no manifest, empty workdir" — keeps
     │   bare-bones dev demo working without any image setup)
     │
  └─ SecretStore::resolve(ctx, manifest.secrets) → SecretBundle
     │  (fails if a required secret is not present in the configured
     │   backend; optional secrets that are absent are skipped)
     │
  └─ Build SandboxSpec
     │   env = manifest.env  ⊕  (literal: secrets / broker: placeholders)
     │   rootfs_source = manifest's rootfs/ or rootfs.ext4 path
     │   resources = manifest.resources or coordinator defaults
     │
  └─ Pool::configure(key, target, vm_spec)   # idempotent
  └─ Pool::checkout(key) → Some(id) | None
       Some: hand the warm sandbox to this session
       None: sandbox.create(spec) inline (cold path)
  └─ tokio::spawn replenish in background
  └─ ENGRAM_SESSION_ID env injection happens at *exec* time, not create —
     pool sandboxes are anonymous until checkout, so session-id env is
     layered on per-exec.
```

---

### Open Q — image registry

Where do warm images live? Options: GCS bucket of OCI tarballs, local Docker registry per host, GCS-backed Container Registry. **Decision deferred to Phase 6.** v1 keeps images on each host's local disk.

### Open Q — eager-clone trick

Ramp's "agents start git-fetching as user types" optimization. Mostly an agent-harness concern, but Engram should expose hooks for it (e.g., `POST /sessions/:id/prefetch` that pulls in flight). Defer to post-v1.

---

## Verification

Each phase has its own verification, summarized:

| Phase | How to verify |
|---|---|
| 1 | `just dev` (subprocess backend on any platform), `curl -X POST :8090/sessions -d '{"repo":"test"}'`, exec a command, verify output. Cold start <5s. |
| 2 | On a Linux host with KVM: `just dev-firecracker`, repeat Phase 1 verification, then idle a session, observe snapshot file appears, evict via `DELETE /sessions/:id/local`, resume → restore in <100ms via UFFD. Run with `engram-storage-gcs`, observe upload + restore-from-cloud. |
| 3 | Stand up 2 hosts. Spawn 10 sessions, verify even distribution. Kill one host with `kill -9`, verify sessions migrate within 30s. |
| 4 | Same code deploys to GCE (with GCP backend) and Hetzner (with static backend). End-to-end smoke test on both. |
| 5 | Run on GCE Spot. Trigger preemption via `gcloud compute instances simulate-maintenance-event`, verify all sessions resume successfully on remaining hosts. |
| 6 | Watch image-builder cron run, verify new image appears in registry, observe warm pool VMs spawning from new tag. |
| 7 | Load test with locust/k6: 100 concurrent sessions, observe P99 cold start, P99 resume, snapshot upload throughput. Chaos test: kill coordinator, kill hosts, kill Postgres briefly. |

End-to-end smoke test for v1 (Phase 1+2):
```bash
# On a GCE n2d-highmem-32 with nested virt enabled, a Hetzner box, or an Apple Silicon Mac for local dev
git clone https://github.com/cortex/engram && cd engram
cp .env.example .env  # fill in Postgres URL, GCS creds
docker-compose up -d
sleep 10

# Spawn a session
SID=$(curl -s -X POST localhost:8090/sessions \
  -d '{"repo":"hello-world","branch":"main"}' | jq -r .session_id)

# Run a command
curl -s -X POST "localhost:8090/sessions/$SID/exec" \
  -d '{"command":"uname -a"}' | jq -r .stdout

# Idle, evict, resume
curl -X POST "localhost:8090/sessions/$SID/snapshot"
curl -X DELETE "localhost:8090/sessions/$SID/local"  # evict from RAM
curl -X POST "localhost:8090/sessions/$SID/resume"   # restore
curl -s -X POST "localhost:8090/sessions/$SID/exec" \
  -d '{"command":"echo still here"}' | jq -r .stdout
```

If that all works, Phase 1+2 are done.

---

## Critical files (to be created in v1)

This is a greenfield project. Phase 1 specifically creates:

- `Cargo.toml` (workspace root)
- `crates/engram-core/src/traits/{cloud,storage,metadata,sandbox}.rs` — trait definitions
- `crates/engram-core/src/types/mod.rs` — shared types
- `crates/engram-core/src/error.rs` — error enums
- `crates/engram-coordinator/src/main.rs` + `src/api/{sessions,exec,health}.rs`
- `crates/engram-host-agent/src/main.rs` + `src/{pool,snapshot,resource}.rs`
- `crates/engram-sandbox-microsandbox/src/lib.rs` — SandboxBackend impl
- `crates/engram-cloud-static/src/lib.rs` — CloudBackend impl (no-op preemption)
- `crates/engram-storage-local/src/lib.rs` — BlobStorage impl
- `crates/engram-postgres/src/lib.rs` — MetadataStore impl
- `deploy/migrations/0001_initial.sql` — schema from this doc
- `deploy/docker-compose.yml` — dev stack
- `README.md`, `LICENSE`, `.github/workflows/ci.yml`

---

## Reference reading

- Stripe Minions parts 1-2 — orchestration patterns, blueprints, devboxes (https://stripe.dev/blog/minions-stripes-one-shot-end-to-end-coding-agents and …part-2)
- Ramp Inspect — sandbox semantics, multiplayer, snapshot lifecycle (https://builders.ramp.com/post/why-we-built-our-background-agent)
- Firecracker design + snapshot docs (https://github.com/firecracker-microvm/firecracker)
- Firecracker UFFD memory backend (`docs/snapshotting/`)
- AWS Lambda's snapshot-based cold starts (Firecracker + REAP-style page prefetch)
- Cloud Hypervisor (https://github.com/cloud-hypervisor/cloud-hypervisor) — alternative VMM if we ever need bigger VMs / live migration
- claw-code's Rust workspace structure (~/test/claw-code/rust/) — pattern reference for crate layout, error handling, no-thiserror approach

---

## Glossary

- **Sandbox**: a Firecracker microVM (production) or per-sandbox subprocess working directory (dev) running an OCI-derived rootfs with the workspace.
- **Session**: a logical conversation/task. May span many sandboxes (resumed after eviction).
- **Snapshot**: a frozen state of a sandbox, restorable on any host. On Firecracker: VM state file + memory file (UFFD-restorable). On the dev backend: tarball of the workdir.
- **Warm pool**: pre-spawned sandboxes (or pre-warmed snapshots) per repo, idle, ready for checkout.
- **Image version**: a built+cached rootfs.ext4 (Firecracker) or OCI tag (dev) for a repo, refreshed every ~30 min by the image baker.
- **Coordinator**: stateless service that schedules sessions onto hosts.
- **Host agent**: per-VM-host daemon managing pool, snapshots, resources.
- **engram-agentd**: small in-guest daemon (Phase 2) that listens on vsock and proxies exec / stdin / stdout. Required because Firecracker has no native exec primitive.
- **Engram**: a stored memory trace. In our system: a persisted session state (snapshot + Postgres history).
