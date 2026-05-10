# Engram — Design & Plan

A self-hosted, open-source orchestrator for ephemeral AI agent sandboxes. Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, FC snapshot lifecycle (UFFD-backed hot resume), multi-host scheduling, and a pluggable cloud abstraction. A subprocess-based dev backend lets the entire orchestration layer run on macOS for fastest-possible iteration; an Apple Silicon backend drives Apple's Virtualization.framework for real microVM isolation locally. Production isolation is always Firecracker.

> Founding design + roadmap. Sections that reflect superseded earlier decisions are marked. ADRs in `docs/adr/` capture the live trajectory:
>
> - **ADR 0001** — sessions are versioned conversations (workspace + transcript externalized to git + Postgres; blob storage removed).
> - **ADR 0002** — engram is a one-shot task runner (no cross-host cold resume; sessions live ↔ FC snapshot lifetime).
> - **ADR 0003** — Apple Silicon backend via Virtualization.framework (clone-based snapshots, virtio-console transport, sub-second cold boot).

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
                  │   - HTTP API + SSE bus       │
                  │   - Scheduler / host registry│
                  │   - Idle evictor             │
                  │   - Owns Postgres metadata   │
                  └───────────────┬──────────────┘
                                  │ bincode-over-WS Frame
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
         ┌─────────┐         ┌─────────┐         ┌─────────┐
         │ Host A  │         │ Host B  │         │ Host C  │
         │ engram- │         │ engram- │         │ engram- │
         │ host    │         │ host    │         │ host    │
         │ agent   │         │ agent   │         │ agent   │
         │   │     │         │   │     │         │   │     │
         │   ▼     │         │   ▼     │         │   ▼     │
         │firecrkr │         │firecrkr │         │firecrkr │
         │ +UFFD   │         │ +UFFD   │         │ +UFFD   │
         │warm pool│         │warm pool│         │warm pool│
         │harness  │         │harness  │         │harness  │
         │  hub    │         │  hub    │         │  hub    │
         └────┬────┘         └────┬────┘         └────┬────┘
              │ git push (workspace)
              ▼
       ┌─────────────────────┐         ┌─────────────────────┐
       │  Git remote         │         │  Postgres (managed) │
       │  (GitHub / GitLab / │         │   - sessions        │
       │   etc.)             │         │   - session_events  │
       │  engram/sessions/<id│         │   - hosts / sandboxes│
       │  branch per session │         │   - image_versions  │
       └─────────────────────┘         └─────────────────────┘
```

Snapshot store lives on each host's local NVMe — an FC `memory.bin` for hot resume, an APFS rootfs clone for VZ's clone-based snapshots. Snapshots are explicitly *not* replicated; a session's lifetime is bounded by its FC snapshot's local lifetime (ADR 0002). Sessions whose snapshots are gone go `Dead`; the workspace lives on as a git branch and the caller forks if they want to continue.

### Component summary

- **`engram-coordinator`**: stateless HTTP service (axum). Owns scheduling, host registry, session metadata, idle evictor, persistent SSE event bus. Backed by Postgres. Multiple replicas re-broadcast events to each other via `LISTEN/NOTIFY` on `session_events` + `host_dead`. `--mode=all` registers a local backend in-process for single-binary dev.
- **`engram-host-agent`**: per-host daemon. Drives sandbox lifecycle and snapshot/restore through `SandboxBackend`. Three impls ship today:
  - `engram-sandbox-firecracker` — Linux + KVM. Production. Firecracker's HTTP-over-Unix-socket API + UFFD-backed memory restore.
  - `engram-sandbox-vz` — macOS Apple Silicon. Apple Virtualization.framework via `objc2-virtualization` bindings. APFS clone-based snapshots. Mac dev with real microVM isolation. (ADR 0003.)
  - `engram-sandbox-process` — anywhere. Subprocesses, no isolation. Fastest iteration loop for orchestration-layer work.
- Maintains warm pool (host-side `PooledBackend` wrapper, agnostic of which backend), `HarnessHub` TCP listener for in-VM harness adapters dialing back, preemption signal handler. Heartbeats `(capacity, warm_pools, local_snapshots, draining)` to the coordinator.
- **`engram-bootstrap`**: in-VM supervisor (lives at `/sbin/engram-bootstrap`). Listens on the host↔guest control transport for `BootstrapLaunch` frames; on each frame, kills the previous harness child and spawns a fresh one. Necessary because the warm-pool spec is agent-blind (per-session argv can't ride on the spec template) and because FC snapshot/restore needs a clean re-spawn point on resume.
- **`engram-agentd`**: in-VM exec daemon. Length-prefixed bincode over the configured transport. Verbs: `Exec` (streaming), `Stat`, `Upload`, `Download`, `Ping`, `Shutdown`. First-frame token handshake (server side) gates non-trivial verbs.
- **`engram-transport`**: backend-agnostic transport trait. `VsockTransport` (FC) and `ConsoleTransport` (VZ); chosen at runtime via `ENGRAM_TRANSPORT` set by the bake's init shim.
- **`engram-image-builder`**: warm-image baker. `Dockerfile` + `engram.toml` → `docker build` → `docker create + export | tar -x` → optional `mke2fs -t ext4 -F -d`. Injects static-musl `engram-agentd` + `engram-bootstrap` + harness binaries + `/sbin/engram-init` shim into the rootfs.
- **`engram-cli`**: ops/admin tool. `engram session {list,get,delete,logs,prompt,log,diff,fork,checkpoint}`, `engram host {list,get,drain}`, `engram image build`.

---

## Source-of-truth model

Four layers of state, with explicit durability guarantees (ADR 0005 supersedes the git-as-durability framing of ADR 0001):

| Layer | Contents | Durability | On loss |
|---|---|---|---|
| **Postgres** | session metadata, `session_events` (conversation log + tool calls), host registry, sealed cold-blob refs | permanent (managed/backups) | hard failure — must guard |
| **Image registry** | bake images + harness packs as OCI artifacts (ADR 0004); host-agents pull on first use into a content-addressable cache | reproducible from Dockerfiles + push pipelines | rebuild |
| **Hot snapshot store** | per-host local NVMe — FC `memory.bin` + state file on Linux, APFS rootfs clones on VZ. Sub-second resume on the same host. | host-local only; not replicated | falls back to cold tier if `blob_present`, otherwise the session goes `Dead` |
| **Cold snapshot store** | tar+zstd of the FC snapshot dir, stored in a `BlobStorage` backend (S3/GCS/local fs); sealed blob URL persisted in Postgres under the deployment KEK | permanent (replicated by the bucket / backed up by the deployment's own policy) | session goes `Dead` (terminal) |

**Design rule**: snapshots come in two tiers, with the cold tier as the cross-host durability primitive. Sessions live ↔ snapshot residency in *either* tier. Hot is the fast path; cold survives host loss, disk-pressure flushing, and operator-initiated drains. The conversation log in Postgres survives independently — historical events are queryable forever — but the agent's in-memory state lives or dies with the snapshot pair.

Git is **not** in this table. Agents that want their work to land in a remote do `git push` themselves inside the sandbox using credentials mounted via `[secrets.GITHUB_TOKEN]` (or SSH key); that's a tool the agent uses, not a layer the platform owns.

---

## Components — detailed

### Coordinator (`engram-coordinator`)

**Responsibilities:**
- Receive session requests (`POST /sessions`). Pick a host, return session id + persistent SSE event stream.
- Maintain host registry. Heartbeats from each host every 5s: capacity (vCPU / memory / disk), warm pool state per `image_version`, local snapshots held, draining flag.
- Track session ↔ host ↔ sandbox routing in Postgres so coordinator restart restores active sessions.
- Drive scheduling decisions: prefer host with snapshot local → host with warm pool match → any host with capacity → fail.
- Drive idle-eviction policy: per-session idle TTL → snapshot + destroy → `Idle` → auto-resume on next request.
- Detect dead hosts via missed heartbeats; race other replicas via `pg_try_advisory_lock`; mark host's sessions `Dead` (ADR 0002 — no cross-host fallover).
- Re-broadcast events between replicas via Postgres `LISTEN/NOTIFY` on `session_events` + `host_dead`.

**Tech:**
- Rust 2021, axum, tokio multi-thread
- sqlx for Postgres (compile-checked queries)
- bincode-over-WebSocket `Frame` protocol (`engram-protocol`) for coordinator ↔ host-agent
- tracing + tracing-subscriber with W3C trace-context per RPC

**State:**
- All in Postgres. Coordinator instances are stateless and horizontally scalable.

### Host agent (`engram-host-agent`)

**Responsibilities:**
- Maintain warm pool of N microVMs per active `image_version` (host-side `PooledBackend` wrapper, agnostic of which `SandboxBackend` is wrapped).
- On checkout: hand a warm VM to a session, replenish pool in background.
- Drive `SandboxBackend` for VM lifecycle (`create`/`destroy`/`exec_stream`/`snapshot`/`restore`/`start_agent`).
- Run snapshot manager (host-local; see below).
- Subscribe to `cloud.preemption_signal()`; on notice fan out best-effort `checkpoint_session` to live sandboxes in parallel with a 25s deadline (Phase 4 Track D).
- Run the `HarnessHub` TCP listener: in-VM harness adapters dial back via `engram-transport` (vsock or virtio-console) → host-side TCP forwarding → `session_events` ingestion.
- Enforce per-VM resource limits via the production backend (Firecracker enforces RAM/CPU/disk at the VMM boundary; the dev backend ignores them with a documented caveat).
- Heartbeat coordinator with capacity + warm pool state + local snapshots.

**Tech:**
- Rust 2021, tokio
- `engram-sandbox-firecracker`: manual HTTP/1.1 over `tokio::net::UnixStream` (no `hyper` — the API is small and the protocol is explicit); the matching `engram-uffd-handler` companion process uses `userfaultfd(2)` for sub-100ms snapshot restore.
- `engram-sandbox-vz`: Apple's Virtualization.framework via `objc2-virtualization` bindings; APFS `clonefile(2)` for snapshots; multi-port virtio-console for the host↔guest control plane (ADR 0003).
- `engram-sandbox-process`: `tokio::process::Command` + per-sandbox cwds; tar+gzip for the dev "snapshot" path.
- bincode-over-WebSocket `Frame` protocol (`engram-protocol`) to the coordinator.

### Snapshot manager (lives inside host agent)

**Two tiers (ADR 0005)**: hot tier on local NVMe for same-host fast resume, cold tier in `BlobStorage` for cross-host durability. ADR 0001 retired the cold tier; ADR 0005 reintroduced it once the premise shifted from "30-second-preemption replication" (infeasible math) to "minutes-budget disk-pressure flush" (trivial math).

- **Hot — FC backend (Linux production)**: `PATCH /vm Paused` + `PUT /snapshot/create` writes a state file + `memory.bin` to local NVMe. Restore via `PUT /snapshot/load` with `backend_type=Uffd` for sub-100ms hot resume on the same host.
- **Hot — VZ backend (macOS dev)**: pause VM → APFS-clone the per-sandbox rootfs into the snapshot dir → resume. The clone is the snapshot. Restore clones it back into a fresh per-sandbox file and cold-boots a new VM (~750 ms total). VZ's native `saveMachineStateToURL`/`restoreMachineStateFromURL` is broken upstream for arm64 Linux guests; ADR 0003 explains.
- **Cold — `BlobStorage` (any backend)**: `engram-host-agent::flush::flush_session` runs `sh -c 'tar -cf - -C <snapshot_path> . | zstd -3 -T0'` and pipes stdout to `BlobStorage::put_streaming`. The blob URL is sealed under the deployment KEK and persisted on the snapshot row (`engram-coordinator::blob::seal_blob_ref`). Resume reverses the pipeline: `BlobStorage::get_streaming` → `zstd -d | tar -xf -` into a fresh per-session staging dir → `SandboxBackend::restore`.

**Hot eviction (idle TTL)**: idle TTL crossed → `idle_evictor` snapshots + destroys the VM, marks the session `Idle`. Next prompt/exec/SSE-subscribe triggers `ensure_active`, which auto-resumes from the local snapshot.

**Cold eviction (disk pressure or admin trigger)**: `engram-host-agent::disk_pressure` polls statvfs; below threshold (default 15% free) it runs the flush primitive on LRU idle sessions. Or operators trigger it explicitly via `POST /api/admin/sessions/:id/flush` / `POST /api/admin/flush-idle`. Implicit + explicit triggers share `flush_session`. Session transitions Idle → ColdEvicted; subsequent `ensure_active` auto-resumes via cold tier.

**Resume dispatcher** (`POST /sessions/:id/resume` and `ensure_active`):
- `Idle` → hot resume (sub-second, snapshot-affinity-routed to the original host)
- `ColdEvicted` → cold resume (download + untar + restore on any host with capacity)
- `Dead` → 410 Gone

**Schema** (`snapshots` table, post-Stage-0 migration `0016`): `session_id, host_id, local_path, image_version, size_bytes, created_at, last_accessed_at, blob_present, replicated_at, wrapped_dek, nonce, ciphertext, key_id`. The four-column sealed-blob-ref quartet matches the `registry_credentials` and `session_secrets` shape from Phase 5b — same `engram-crypto::CredCipher` pipeline.

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

### ~~`BlobStorage`~~ — retired (ADR 0001)

Earlier drafts hung the snapshot replication subsystem off a `BlobStorage` trait with `engram-storage-{gcs,s3,local}` impls. ADR 0001 retired the entire subsystem (replication driver, cold-tier fetch, the trait, all three backends — ~1500 LOC) once the durability story moved to git + Postgres. ADR 0002 then made session lifetime host-local-bounded by design, so the cold-tier fallback would have had no caller anyway. Image distribution (Phase 5) will use a Docker registry, reusing standard tooling rather than rebuilding bespoke object-storage pipelines.

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

The VMM seam. Production = Firecracker on Linux. Dev = subprocesses (anywhere) or Apple Virtualization.framework (Mac). Future backends (Cloud Hypervisor, raw libkrun, Kata) plug into the same trait if we ever need them.

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

- **`engram-sandbox-firecracker` (production, Linux)** — drives Firecracker over its HTTP-over-Unix-socket API. Each sandbox owns a Firecracker process, a per-VM vsock UDS, and (forthcoming) a TAP device. Snapshot via `PATCH /vm Paused` + `PUT /snapshot/create`. Restore via `PUT /snapshot/load` with either `backend_type=File` or `backend_type=Uffd` (lazy paging via the `engram-uffd-handler` companion process — sub-100ms resume regardless of guest RAM size). All `SandboxBackend` methods exercised by integration tests against real microVMs. Production hardening (jailer, TAP networking, broker-mode HTTPS proxy, `SendCtrlAltDel` graceful shutdown) lands in Phase 6.
- **`engram-sandbox-vz` (Mac dev, macOS Apple Silicon)** — drives Apple Virtualization.framework via `objc2-virtualization` bindings. Multi-port virtio-console for the host↔guest control plane (universal kernel support, no `CONFIG_VIRTIO_VSOCKETS=y` requirement). Snapshots are APFS rootfs clones (`clonefile(2)` ~50 ms even at 1.7 GB), since VZ's native `saveMachineStateToURL`/`restoreMachineStateFromURL` is broken upstream for arm64 Linux guests. Cold boot 562 ms; cold resume 746 ms. Same `engram-bootstrap` + `engram-agentd` + harness binaries that run on FC. ADR 0003.
- **`engram-sandbox-process` (dev, anywhere)** — runs commands as plain host subprocesses, each rooted in a per-sandbox working directory. No isolation, no resource enforcement. Fastest-possible iteration loop for orchestration-layer work that doesn't need to exercise the in-VM code paths. `snapshot()` is a tarball of the workdir. **Never use in deployment.**

#### In-guest agent (`engram-agentd`)

Firecracker has no "exec a command in a running guest" primitive. Production rootfs images include `engram-agentd` (`crates/engram-agentd`) — a small Rust binary baked into `/sbin/engram-agentd` that listens on AF_VSOCK port 1024 and proxies exec / stdin / stdout for the host agent. `SandboxBackend::exec_stream` on the Firecracker backend connects to Firecracker's vsock proxy at `<vsock_uds>`, performs the `CONNECT 1024\n` → `OK <peer_port>\n` handshake, sends a `WireExecRequest`, and streams `WireExecEvent`s back. Full design in the [In-guest agent](#in-guest-agent-engram-agentd) section below.

---

## Communication architecture

This section documents the design for **coordinator ↔ host-agent** and **host ↔ guest** communication. Both channels are live: host-to-guest via vsock to `engram-agentd` (exercised by `tests/exec_real_vm.rs`), coordinator-to-host-agent via the bincode-over-WebSocket `Frame` protocol in `engram-protocol` (Phase 3 — `--mode=all` keeps the in-process embedding path for single-binary dev).

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

A small Rust binary baked into every Firecracker rootfs at `/sbin/engram-agentd`, started at guest boot by a tiny init shim (`/sbin/engram-init`). Bridges the gap between Firecracker (which has no exec primitive) and the host agent. Lives at `crates/engram-agentd`.

```text
            host-agent                            ┌── guest VM ──────────┐
                │                                 │  PID 1: /sbin/engram-init
                │   PUT /vsock { guest_cid: N,    │   ↓ (mount /proc, /sys,
                │                uds_path: U }    │      /dev; exec agentd)
                ├────────────────────────────────►│  engram-agentd       │
                │                                 │  vsock listen :1024  │
                │   connect to U (host UDS)       │                      │
                │   write "CONNECT 1024\n"        │                      │
                │ ◄──── "OK <peer_port>\n" ──────┤                      │
                │   WireExecRequest               │  ← spawn child       │
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

### Protocol (`engram-agentd::proto`)

One connection per exec. Length-prefixed bincode: each frame is `[u32 BE length][bincode body]`. Bincode (rather than CBOR/JSON) because both sides are Rust, we own both schemas, and `Vec<u8>` encodes byte-for-byte — important on the hot stdout/stderr path where JSON's array-of-ints would inflate 3-5×.

```rust
// crates/engram-agentd/src/proto.rs

#[derive(Serialize, Deserialize, ...)]
pub struct WireExecRequest {
    pub command: Vec<String>,                // argv; command[0] is the program (no shell)
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    pub timeout_ms: Option<u64>,             // SIGKILL after this; reports Exit(None)
}

#[derive(Serialize, Deserialize, ...)]
pub enum WireExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(Option<i32>),                       // None = signalled / timed out
}
```

Conversation shape: host writes one `WireExecRequest`, agent streams 0+ `Stdout`/`Stderr` events, terminates with exactly one `Exit`, then closes. Multiple in-flight execs use multiple connections (FC's vsock proxy multiplexes them); single-stream demuxing isn't worth the complexity at the in-guest exec scale.

### Authentication — TODO

The agent currently does not authenticate connections — anything that can talk to the vsock UDS gets to spawn processes. This is acceptable for now because:

- Firecracker's vsock proxy is owned by the host's `engram-host-agent` (the same process driving the FC HTTP API)
- The proxy UDS lives in the host-agent's work_dir with default permissions
- Multi-tenant deployments aren't a v1 goal (single-org assumption)

Production hardening will add a first-frame token handshake. Token injection options under consideration: kernel cmdline (`engram_token=...` in `BootSource.boot_args`, visible via `/proc/cmdline` to in-guest readers — simplest), virtio-fs single-file mount (better hygiene, more setup), per-connection token written to vsock UDS by the host before the agent's accept (best — token never persists in the guest). Tracked under Phase 6 (production operability).

### Lifecycle

1. **Init** — kernel boots, exec's `init=/sbin/engram-init`. The shim mounts /proc, /sys, /dev, then exec's `engram-agentd --vsock-port 1024`.
2. **Host connects** — `FirecrackerBackend::exec_stream` opens the FC vsock UDS, writes `CONNECT 1024\n`, reads back `OK <peer_port>\n`.
3. **Per-exec** — host writes a `WireExecRequest`; agent spawns the child, streams events back, sends `Exit`, closes the connection.
4. **Shutdown** — host sends SIGKILL to the firecracker process (today). A graceful path via `SendCtrlAltDel` + agent-side handler lands when the in-guest agent supports more than just exec.

### Distribution

`engram-agentd` is baked into every Firecracker rootfs at `/sbin/engram-agentd`, plus `/sbin/engram-init` (the init shim that brings up just enough kernel plumbing for the agent to talk vsock). Both injected by `engram-image-builder` when `BuildRequest.agent_injection` is set; the binary is built statically against musl (`x86_64-unknown-linux-musl`, release mode) so it runs in any base image regardless of the rootfs's libc / dynamic-linker layout.

Initial scope is exec-only — `Stat`/`Upload`/`Download`/`Ping`/`Shutdown` verbs from the original design are deferred. They'll land when the surface is needed (file upload for snapshot transfer, ping for liveness, etc.).

Versioning: TODO. Two consumers exist now (the agentd vsock proto + the Phase 3 coordinator-to-host WS proto in `engram-protocol`). Both are unversioned; first-frame version negotiation lands with the broader auth/handshake work in Phase 6.

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
    │       ├── traits/                # CloudBackend, MetadataStore, SandboxBackend
    │       ├── types/                 # SessionId, HostId, SnapshotMetadata, etc.
    │       └── error.rs
    ├── engram-coordinator/            # binary: HTTP service + scheduler + idle evictor
    ├── engram-host-agent/             # binary: per-host daemon
    ├── engram-image-builder/          # binary + library: image baker (Directory / Ext4 modes)
    ├── engram-cli/                    # binary: ops/admin
    ├── engram-agentd/                 # binary + library: in-guest exec daemon (transport-agnostic)
    ├── engram-bootstrap/              # binary: in-guest supervisor (handles harness re-spawn)
    ├── engram-uffd-handler/           # binary + library: userfaultfd page-fault handler
    ├── engram-transport/              # transport abstraction (vsock for FC / virtio-console for VZ)
    ├── engram-harness-proto/          # wire types (HarnessEvent / HarnessCommand)
    ├── engram-harness-noop/           # first-party test harness
    ├── engram-harness-claude/         # Claude Code adapter (reference)
    ├── engram-sandbox-firecracker/    # SandboxBackend driving Firecracker (Linux production)
    ├── engram-sandbox-vz/             # SandboxBackend driving Apple VZ (macOS Apple Silicon dev)
    ├── engram-sandbox-process/        # SandboxBackend using host subprocesses (dev only)
    ├── engram-cloud-gcp/              # impl of CloudBackend for GCE
    ├── engram-cloud-static/           # impl of CloudBackend for bare-metal/Hetzner
    ├── engram-cloud-mock/             # impl of CloudBackend for tests
    ├── engram-secrets-dev/            # SecretStore (env / dotenv)
    ├── engram-secrets-gcp/            # SecretStore (GCP Secret Manager)
    ├── engram-postgres/               # impl of MetadataStore for Postgres
    └── engram-protocol/               # bincode-over-WS Frame protocol (coordinator <-> host)
```

(`engram-storage-{gcs,s3,local}` retired with ADR 0001.)

**Key dependency choices** (informed by exploration of claw-code patterns + 2026 Rust ecosystem):

- async runtime: `tokio` 1.x with `rt-multi-thread`
- HTTP server: `axum` 0.7
- coordinator ↔ host RPC: bincode + tokio-tungstenite (no tonic/gRPC — internal protocol)
- Postgres: `sqlx` with compile-checked queries
- HTTP client: `reqwest` 0.12 with rustls-tls
- macOS VZ bindings: `objc2-virtualization` v0.3 (pure-Rust ObjC bindings)
- Observability: `tracing` + `tracing-subscriber`
- Errors: custom enums per crate, no `thiserror`/`anyhow`
- Edition 2021, `forbid(unsafe_code)` per-crate (relaxed for `engram-sandbox-vz` where ObjC bridging needs it)

---

## Phased implementation plan

Order is deliberate: each phase produces something runnable end-to-end. Don't build pluggability before there's something to plug into.

### Phase 1 — Orchestration layer end-to-end on the dev backend ✅

**Goal**: one binary, one host, one repo, end-to-end session, runnable on macOS Apple Silicon.

- ✅ `engram-coordinator` HTTP API: `POST /sessions`, `GET/DELETE /sessions/:id`, `POST /sessions/:id/exec` + `/exec/stream` (SSE), `POST /sessions/:id/snapshot/resume`, `DELETE /sessions/:id/local`, `GET /sessions/:id/events` (SSE replay via `?since=N` / `Last-Event-ID`).
- ✅ Bearer-token auth middleware (constant-time compare, `/healthz` exempt).
- ✅ `engram-sandbox-process` real implementation: per-sandbox cwd, real subprocess exec, tarball-based snapshot/restore.
- ✅ Pluggable `SecretStore` (env / dotenv / GCP-Secret-Manager-stub) with `Literal` and `Broker` modes (the broker proxy itself lands in Phase 6).
- ✅ `engram-cloud-{static,gcp,mock}`, `engram-storage-{local,gcs-stub,s3-stub}`, `engram-postgres`.
- ✅ SandboxRegistry + lifecycle wiring: create allocates a sandbox, exec routes through it, delete tears it down.
- ✅ Warm-pool data structure with backend-driven replenish.
- ✅ Postgres schema + migration runner.
- ✅ Persistent session-event log + SSE bus, `Last-Event-ID` reconnect.
- ✅ Per-exec `ExecRusage { wall_ms, peak_rss_kb?, user_cpu_ms?, sys_cpu_ms? }` on `ExecCompleted` events and `/exec` responses.
- ✅ ~290 workspace tests across the unit + integration surface.

**Deliverable**: `just dev` brings up Postgres + coordinator (subprocess backend) on a Mac. `curl POST /sessions` returns a session, `POST /sessions/:id/exec` round-trips real stdout/stderr/exit-status.

### Phase 2 — Firecracker integration (Linux production path) ✅

**Goal**: production-grade isolation, snapshot-evict mechanic working end-to-end.

**Done:**

- ✅ `engram-sandbox-firecracker` real implementation
  - Manual HTTP/1.1 over `tokio::net::UnixStream` (no `hyper`; the protocol is small and explicit)
  - VM config: `PUT /machine-config` + `/boot-source` + `/drives/rootfs` + `/vsock`, then `PUT /actions InstanceStart`
  - Lifecycle: SIGKILL on destroy via the spawned `Child` (graceful `SendCtrlAltDel` deferred to Phase 6)
- ✅ `engram-agentd` (in-guest exec daemon, vsock listener) and `engram-init` (boot shim that mounts /proc /sys /dev and exec's the agent). Built static-musl via `x86_64-unknown-linux-musl`; injected into rootfs by the image baker.
- ✅ Snapshot/restore via Firecracker API
  - Take: `PATCH /vm Paused` → `PUT /snapshot/create { snapshot_path, mem_file_path, snapshot_type: "Full" }` → `PATCH /vm Resumed`
  - Restore: `PUT /snapshot/load` with both `backend_type=File` (synchronous, simple) and `backend_type=Uffd` (lazy paging)
- ✅ **UFFD memory backend** (`engram-uffd-handler`) — separate companion process. Receives the userfaultfd via SCM_RIGHTS, mmaps `memory.bin` (PROT_READ + MAP_PRIVATE + MAP_POPULATE), services `Pagefault` events with `UFFDIO_COPY`. Sub-100ms resume regardless of guest RAM. Confirmed against upstream's `on_demand_handler.rs`.
- ✅ Image baker `Format::Ext4` mode — `mke2fs -t ext4 -F -d <staging> <out>.ext4`. No loopback mount, no root.
- ✅ Coordinator config plumbing — `--kernel-image-path` / `ENGRAM_KERNEL_IMAGE_PATH`; `FirecrackerConfig::{kernel_image_path, default_boot_args, restore_mode, uffd_handler_bin}`.
- ✅ 5 integration tests against real microVMs on the GCP dev VM — `boot`, `lifecycle`, `snapshot`, `snapshot_uffd`, `exec_real_vm` (full bake → boot → vsock CONNECT → exec → assert stdout).

**Done in follow-up PRs:**

- ✅ **Snapshot replication path** — coordinator-side driver in `engram-coordinator::replication` polls `MetadataStore::list_pending_replications` every ~30s, tar+zstd-3-compresses each pending snapshot's `local_path`, streams to `BlobStorage::put`, then stamps `blob_url` + `replicated_at` via `mark_snapshot_replicated`. Once a snapshot is replicated the host-side `SnapshotManager`'s LRU layer can evict it under disk-cap pressure (LRU itself is dormant code today, activates when host disk fills). Coordinator-driven works for any deployment where coord can read the host's filesystem (`--mode=all`, co-located fs); true multi-machine adds a host-side `UploadSnapshot` RPC. The original Phase 2 TODO on `engram-host-agent::snapshot::replicate` is now a no-op pass-through pointing at the coord-side driver.

- ✅ **Real `engram-storage-{gcs,s3}` SDK calls** — `engram-storage-s3` now uses `aws-sdk-s3` v1 (default credential chain, optional region + endpoint override for MinIO / R2 / other S3-compatibles); `engram-storage-gcs` uses `google-cloud-storage` v0.24 with ADC. Both implement the full `BlobStorage` surface (`put`/`get`/`delete`/`exists`/`list`) including pagination on `list_objects` and `NotFound` error mapping (404 → `StorageError::NotFound`, idempotent delete on missing). `put` collects the inbound stream into a `Vec<u8>` before handing to the SDK — fine for dev-tier ProcessBackend tarballs; multi-GB Firecracker `memory.bin` uploads will switch to streaming bodies (S3 `ByteStream::from_body_1_x`, GCS resumable upload) once we plumb content-length through the snapshot wire path. Each crate ships an `#[ignore]`'d live round-trip test gated on `ENGRAM_TEST_{S3,GCS}_BUCKET` so CI / dev can verify against a real bucket without forcing every workspace test run to need cloud credentials.

- ✅ **First-frame token auth on `engram-agentd`** (server side) — new `WireHandshake { token, agent_version }` + `WireHandshakeAck { ok, message }` wire types. `serve_connection(stream, expected_token: Option<String>)` reads the handshake first when a token is configured, replies with a typed ack, and only then accepts the `WireExecRequest`; mismatch surfaces as `io::ErrorKind::PermissionDenied` after writing the rejection. Token resolution chain: `--token <T>` CLI arg → `ENGRAM_AGENT_TOKEN` env → `engram_token=<T>` on `/proc/cmdline` (Linux only — the production injection path uses Firecracker's `BootSource.boot_args`). Constant-time compare via the same `ct_eq` shape used in `engram-coordinator::api::auth`. Fully back-compat: `expected_token = None` (the default) skips the handshake entirely so older hosts that don't know about it keep working. Host-side wiring (`FirecrackerBackend` sending `WireHandshake` from `BootSource.boot_args`-derived per-VM tokens before `WireExecRequest`) is the next follow-up — it touches per-sandbox state plumbing in the backend that's better landed alongside the broader Firecracker production hardening.

- ✅ **Full agentd verb set** — `WireRequest` envelope wraps the original `WireExecRequest` (`WireRequest::Exec(req)`) and adds `Stat { path }`, `Upload { path, bytes, mode? }`, `Download { path }`, `Ping`, and `Shutdown`. Each non-streaming verb gets exactly one `WireResponse` reply (`Stat`, `UploadOk`, `Download`, `Pong`, `ShutdownAck`, or typed `Error { kind, message }` mirroring `io::ErrorKind`); Exec keeps its existing streaming-`WireExecEvent` shape. Single-frame body cap is the existing `MAX_MSG_BYTES` (16 MiB) — Upload/Download bigger than that errors instead of silently truncating; multi-frame chunked transfer is a follow-up if anyone hits the limit. Wire-format change for the Exec path (callers wrap in `WireRequest::Exec`); `engram-sandbox-firecracker::drive_exec_protocol` updated alongside, and the Phase 6-deferred `SendCtrlAltDel` graceful shutdown will pair with `WireRequest::Shutdown` (agent currently acks then leaves; pairing with `PUT /actions SendCtrlAltDel` actually stops the VM).

**Still deferred** (rolled into Phase 6 production hardening, since they're orthogonal to "the trait surface works"):

- `firecracker-jailer` integration (drop privileges, chroot, cgroups, seccomp, `/dev/kvm` fd passing)
- TAP networking + per-VM IP allocation
- **Broker-mode HTTPS proxy** — sized as its own focused work, not a tail-of-session add-on. Implementation needs: per-deployment managed CA cert (baked into the image trust store at bake time, see "Inject the per-deployment broker CA cert into the rootfs's trust store" below); per-session listener that terminates TLS using a hostname-derived spoofed leaf cert signed by the managed CA; HTTP request inspection that scans headers (`Authorization: Bearer engram_ph_*`), URL params, and request bodies for `engram_ph_<session>_<hash>` placeholders; lookup table per session mapping placeholder → real secret value; allow-host enforcement (refuse outbound to anything not in the manifest's `schema.allow_hosts`/`allow_host_patterns`); re-encrypt and forward to upstream. Per-session scope so one session's leaked-via-side-channel placeholder can't be redeemed by another session. Significant project (~1500 LOC + cert management); a half-measure HTTP-only proxy delivers `allow_hosts` enforcement but not actual substitution and isn't worth landing as a placeholder.
- Network policy enforcement via iptables/nftables at the TAP boundary
- `SendCtrlAltDel` graceful shutdown (agent-side shutdown handshake is done — the host's pairing piece lives here)

### Phase 3 — Multi-host coordinator ✅

**Goal**: two hosts working together; new hosts come online with zero coordinator-side config.

**Done:**

- ✅ **Split binaries** — `engram-host-agent` runs separately, dials coordinator over WebSocket. `--mode=all` registers the local backend in-process so `just dev` stays single-binary.
- ✅ **Wire transport** — bincode-encoded `Frame` enum (`Request`/`Response`/`Stream`/`Notify`) over WebSocket binary messages. Hand-rolled `request_id` demuxer routes Responses to `oneshot`s and Stream items to per-id `mpsc`s. `engram-protocol::{wire,codec,client,server}`. **Decision flip from the original design**: bincode-over-WS replaces the originally-planned tonic+gRPC — reuses the existing `engram-agentd` framing pattern and avoids a separate `.proto` file + `build.rs` for an internal-only channel.
- ✅ **`/api/hosts/connect` axum handler** under the existing bearer-auth middleware. axum↔tungstenite Message bridge at the boundary so `engram-protocol` stays axum-agnostic.
- ✅ **Host-agent dialer** — exp-backoff reconnect, `NotifyKind::Hello` first frame, periodic heartbeat. `coordinator_endpoint` + `coordinator_token` config.
- ✅ **`HostRegistry`** — coordinator-side, implements `SandboxBackend`. Tracks `sandbox_id → host_id` ownership for routing. Heartbeat-derived per-host state (`HostState { capacity, warm_pools, local_snapshots, draining }`) populated by the WS supervisor.
- ✅ **Real scheduler** — `pick_for_session(ctx)` ranks: snapshot affinity → warm-pool match → capacity-fit → any non-draining fallback. `BackendError::NotSupported("no host available")` when everything's full. `assign_session_host` wired into `create_session`.
- ✅ **Cold-tier blob restore unblocked** — `api/snapshot.rs` pulls from `blob_url` into a coordinator-local cache when `local_path` is None, then routes through the scheduler. Snapshots now record `host_id` so the next resume hits the snapshot-affinity branch.
- ✅ **Coordinator HA via `LISTEN/NOTIFY`** — `append_session_event` fires `NOTIFY session_events`; `pg_listener::spawn` task in each replica re-broadcasts into its local `SessionEventBus`. SSE subscribers see events regardless of which replica produced them.
- ✅ **Session migration (operator-initiated)** — `SessionStatus::PendingReassign` variant, `POST /sessions/:id/migrate { host_id? }` clears `host_id` and transitions; next `/resume` reschedules. Refuses to migrate without a snapshot to come back from.
- ✅ **`GET /api/hosts` + `GET /api/hosts/:id` + `POST /api/hosts/:id/drain`** — surface heartbeat state for ops + the CLI.
- ✅ **`engram host list/get/drain`** CLI commands.
- ✅ 322 workspace tests pass (up from 292), with new wire-loopback (5), AppState-via-wire integration (4), and scheduler-ranking (4) suites.

**Done in follow-up PRs:**

- ✅ **Dead-host auto-detector** — `crates/engram-coordinator/src/dead_host.rs`. Polls `MetadataStore::list_stale_hosts(threshold)` every ~10s, races other replicas via `pg_try_advisory_lock(hashtext('dead-host:' || host_id))`, atomically marks the host `Dead` + transitions its sessions to `PendingReassign` with `host_id` cleared (`MetadataStore::mark_host_dead_and_reassign_sessions`), emits `pg_notify('host_dead', host_id)` so other replicas drop their `HostRegistry` entry via `pg_listener`, and emits `SessionEvent::StatusChanged{Active → PendingReassign}` for SSE subscribers. **`ExecCompleted` synthesis turned out to be unnecessary**: dropping the host's `RemoteSandboxBackend` cascades through the WS demuxer's close path → per-exec stream channel drops → SSE handler's event loop ends naturally and emits `ExecCompleted{exit_status: None}` from its existing tail logic. `0003_hosts_heartbeat_index.sql` adds a partial index on `(status, last_heartbeat_at) WHERE status IN ('ready','draining')` for the polling query. 6 Mock-based unit tests in `tests/dead_host_mock.rs` lock down the trait-layer semantics; the detector loop's advisory-lock dance is Postgres-specific and falls back to the live-Postgres integration test below.

- ✅ **Host-side Pool relocation + real heartbeat data** — new `engram-host-agent::pooled_backend::PooledBackend` wraps any `SandboxBackend` and adds opportunistic warm-slot checkout on `create()`. The `HostAgent` (and `--mode=all` in coordinator main) installs this wrapper, and the dialer's `heartbeat_provider` closes over it so each tick ships real `(ready, target)` counts per `image_version`. Coordinator-side `AppState.pool` is gone; `api/sessions.rs::create_session` just calls `host_registry.create_for_session` and lets the picked host's `PooledBackend` decide whether to checkout from its pool or create fresh. Scheduler's warm-pool branch matches on `image_version` only (the diagnostic `repo` field on `WarmPoolReport` is informational) — two repos sharing an image share warm slots, which is correct: the pool's job is to amortise per-image create cost, not per-repo isolation.

- ✅ **Persist `sandbox_id` on sessions for restart-resilient routing** — `0004_sessions_sandbox_id.sql` adds the column; `MetadataStore::assign_session_sandbox` wires it into the create / resume / evict / delete / migrate / dead-host paths. On coordinator startup, `repopulate_routing` reads active sessions and seeds the in-memory `SandboxRegistry` (`session_id → sandbox_id`) and `HostRegistry.sandbox_owner` (`sandbox_id → host_id`) so a coordinator restart no longer leaves Active sessions with "no live sandbox" errors. Once the host dials back in via `/api/hosts/connect` and registers its `RemoteSandboxBackend`, routing resumes for those sessions without further intervention. **Scope simplification**: the original deferred-list plan called for a separate `host_assignments` append-only log + `Hello { last_seen_assignment_idx }` delta-on-reconnect flow, but the simpler `sessions.sandbox_id` column delivers the same correctness outcome (Active sessions survive coord restart) with much less surface. The append-only log was solving a different problem (per-host assignment audit trail) that isn't load-bearing today.

- ✅ **W3C tracing context on the wire** — `Frame::Request` carries a `TraceContext { trace_id: [u8; 16], span_id: [u8; 8] }`. The coordinator's `client.rs` generates fresh ids per outgoing RPC and logs them at debug; the host's `server.rs::handle_request` opens a `tracing::info_span` with `trace_id`/`span_id`/`req_id`/`kind` fields so backend code (e.g. `ProcessBackend`, `FirecrackerBackend`) inherits them on every log line. Without an OTel collector wired in, this stitches both sides' logs together via grep; with `tracing-opentelemetry` added later, the same fields plug straight into a real distributed trace. Wire-format change — coordinator + host need to be on the same version (intentional: this is an internal protocol with no version negotiation yet, that's tracked in Phase 6).

- ✅ **`tests/ha_listener.rs` against live Postgres** — two `#[ignore]`'d integration tests in `crates/engram-coordinator/tests/ha_listener.rs`: `cross_replica_event_fan_out` builds two `AppState`s sharing one Postgres, subscribes on coord-A, emits via coord-B, and asserts coord-A's subscriber sees the byte-identical event within 2s; `append_session_event_fires_pg_notify` does a targeted check that `append_session_event` actually emits a `NOTIFY` payload with the correct `(session_id, idx)` shape. Gated behind `ENGRAM_TEST_DATABASE_URL`; run with `docker compose -f deploy/docker-compose.dev.yml up -d postgres && ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram cargo test -p engram-coordinator --test ha_listener -- --ignored`.

**Still deferred (rolled into Phase 6):**

- **Production hardening** — TLS for the WS channel, `HostStatus::Disconnected` (network blip vs. dead distinction), backpressure tuning past the default 64-frame mpsc capacity. Lands with Phase 6.

**Deliverable**: stand up coordinator (2 replicas) + 3 hosts; sessions distribute. Kill one host with `kill -9 firecracker-pid` and observe sessions migrate within 30s. Restart a coordinator replica; client SSE streams transparently survive.

### Phase 4 — Hot suspend + harness protocol ✅

> **Note**: Phase 4 was originally framed as "agent-first durability via git checkpoints" (ADR 0001 + 0002). Phase 6 / ADR 0005 retired that framing — git is no longer the platform's workspace durability primitive; the cold-tier `BlobStorage` it deleted is back. The hot-suspend / harness-protocol / preemption-drain pieces of Phase 4 stayed; Tracks 0 (blob removal) + C (checkpoint primitive) + F (`engram session {log,diff,fork,checkpoint}`) were unwound.

**Goal (post-amendment)**: hot-suspend mechanics + the harness protocol. Phase 4 was originally framed as "drain VM memory snapshots to GCS in the 30s GCP preemption window." That model is mathematically infeasible for realistic workloads (5 sandboxes × 8 GiB ≫ 30s × 250 MB/s) — that math holds. ADR 0005's reframe: the relevant deadline is *disk-pressure flushing* (minutes, not seconds), which makes blob-tier durability viable again. See Phase 6.

ADR 0002 narrowed the contract: **engram is a one-shot task runner**. ADR 0005 amends the durability boundary from "session lives ↔ FC snapshot exists on its origin host" to "session lives ↔ snapshot exists somewhere (hot tier or cold tier)." The one-shot semantics still hold — sessions don't infinitely migrate, hot resume stays same-host, `Dead` is terminal — the new shape just moves the Dead boundary to "snapshot lost from both tiers."

**Storage split (current shape — see Phase 6 / ADR 0005 for the supersession trajectory)**:

- **Postgres `session_events`** is the conversation source of truth. Tool-call-grain harness events land here; the SSE bus, Web UI, Slackbot consume from this stream.
- **Hot snapshot store** (per-host local NVMe) for sub-second same-host resume.
- **Cold snapshot store** (`BlobStorage` — S3/GCS/local fs) for cross-host durability under disk pressure / admin flush. Sealed under the deployment KEK in Postgres.
- ~~Git remote~~ — retired by ADR 0005. Agents that want to push code do it themselves inside the sandbox via mounted credentials; the platform never runs `git push`.

**Two snapshot mechanisms, distinct purposes**:

| | Hot snapshot (Firecracker) | Cold checkpoint (git) |
|---|---|---|
| Mechanism | `PATCH /vm Paused` + `PUT /snapshot/create`, memory.bin to local NVMe | `git add/commit/push` from inside the sandbox |
| Resume cost | Sub-second (UFFD lazy page-in, same host) | Few seconds (fresh sandbox + git fetch + reset) |
| Cross-host? | No — memory tied to that host's RAM | Yes |
| Right for | Idle eviction → pack hosts. Warm pool. | Spot preemption. Long-term parking. Cross-host migration. Forking. |

**Auto-checkpoint cadence**: one commit per *completed agent run*, not per tool call. The harness emits `HarnessEvent::Idle` / `RunCompleted` when it finishes a prompt; the EventSink fires `checkpoint_workspace_only` for Git sessions, which runs `git add -A && git commit && git push origin engram/sessions/<id>`. Per-tool-call commits would drown the branch in dozens of empty/near-empty pushes per run; one-per-completed-run aligns with the unit a human reviews via `engram session diff <id>` or `engram session pr <id>`.

**Smart-bootstrap composition with warm pool**:

- Phase 5's image baker bakes warm-pool images that include a clone of the writable repo at "bake SHA" + dependencies installed + long-lived daemons warmed (gradle daemon, language servers, Firecracker memory snapshot of post-init state).
- Session create: warm pool delivers a sandbox with `/workspace` already cloned at bake SHA. Bootstrap runs `git fetch origin && git reset --hard origin/<branch>` — milliseconds for sessions on the same `<branch>` the image was baked from; small diff for resumed sessions on `engram/sessions/<id>`.
- Session resume on a different host: same image, fresh sandbox, smart-bootstrap detects the existing clone via `git remote get-url origin`. If origin matches: fetch + reset. If origin diverged or `/workspace` is empty: `git init . && git remote add origin <url> && git fetch + reset --hard FETCH_HEAD`. Either path lands the workspace at the checkpoint branch HEAD without a full re-clone.
- Resume always uses the *same image_version* the session was originally created with, so the bake SHA → checkpoint branch diff is bounded and predictable.

**Resume contract**:

- **Hot resume** (FC snapshot intact, same host): in-memory state preserved. `POST /sessions/:id/resume` routes to the snapshot's host; sub-second.
- **Cold resume** (host gone, no FC snapshot): fresh sandbox on the same image, smart-bootstrap to the checkpoint branch. In-memory state lost; workspace + transcript reconstructed. Caller-driven — preempted sessions land in `PendingReassign` and stay there until `POST /sessions/:id/resume`.
- **Read-only / Local sessions**: no checkpoint branch; cold resume is unavailable. Caller forks via `engram session fork` if they want to continue from the workspace state.

**Tracks** (see ADR 0001 for the trajectory + plan file for granular breakdown):

- Track 0 — done. Blob storage removed; `engram-storage-{local,gcs,s3}` retired; replication subsystem deleted.
- Track A — done. `engram-harness-proto` wire types; `engram-harness-noop` + `engram-harness-claude` adapters; in-VM `engram-bootstrap` supervisor (kept as a permanent supervisor process so post-snapshot/restore reattach works without re-spawning init); host-side `HarnessHub` forwards events into `session_events`.
- Track B — done. Idle evictor (`engram-coordinator::idle_evictor`) cold-checkpoints, takes a hot FC snapshot, destroys the sandbox, marks `Idle`. `ensure_active` auto-resumes on the next `exec` / `exec_stream` / SSE-subscribe / `POST /sessions/:id/prompt`.
- Track C — done. `RepoUrl` / `SessionKind` types and schema migration; `checkpoint_session` primitive (harness round-trip + git push) and `checkpoint_workspace_only` (auto-cadence — skips the harness round-trip); `POST /sessions/:id/checkpoint`; auto-checkpoint on `HarnessEvent::Idle` / `RunCompleted`. (`smart_bootstrap_to_branch` was retired by ADR 0002 along with cross-host cold resume.)
- Track D — done. Host-agent consumes `cloud.preemption_signal()`; on notice fans out `checkpoint_session` to every live sandbox in parallel with a 25s deadline, drops them, signals the coord, accepts VM death.
- Track E — done. `local://hello` quickstart, CLI surfaces `session_kind` / `checkpoint_branch`, ADR 0001 + ADR 0002 capture the trajectory.
- Track F — done (modulo F.6). `GET /sessions/:id/log?kind=conversation|workspace`, `GET /sessions/:id/diff?vs=<ref>`, `POST /sessions/:id/fork`. CLI verbs ship at `engram session {log,diff,fork,checkpoint,prompt}`. F.6 (`POST /sessions/:id/pr` — GitHub-only) is deferred until the prod auth path lands; the runtime API for PR creation is identical whether the session token comes from a per-session token or an App installation token (Phase 6).
- ADR 0002 cleanup — done. Cross-host cold resume + `?from_event_idx` resume-replay retired in favor of explicit one-shot semantics. `SessionStatus::PendingReassign` collapsed into `SessionStatus::Dead` (terminal); `POST /sessions/:id/migrate` removed; `HTTP 410 Gone` on resume against an invalidated snapshot.

**What Engram does NOT solve** (explicit contract):

- **Idempotency for non-idempotent tool calls.** A session that calls `slack.post()` and gets preempted may, on resume, redo the post. Tool wrappers must use idempotency keys derived from `(session_id, tool_call_id)`. Same model as Modal, AWS Step Functions.
- **Fork determinism for sessions with side effects.** Forking from a checkpoint that pre-dates a `db.delete_user(123)` call replays a conversation into a world where 123 is *already* deleted. Forking is best-suited to code-edit tasks.
- **Mid-tool-call work preservation.** A 5-minute `pytest` running when preemption hits has no checkpoint since the last tool call returned. On resume, the agent reruns it. Acceptable; checkpoints land at run boundaries (Idle/RunCompleted), not within tool calls.
- **Cross-vendor agent compatibility out of the box.** Each agent (Claude Code, Codex, Aider) needs an adapter implementing the harness protocol. Phase 4 ships Claude Code as the reference; others land as adopters need them.

**Deliverable**: ✅ delivered (the engineering portion). Stand up `deploy/gcp/` Terraform on a real GCP project lands later as Phase 6 prep. The Claude Code adapter (`engram-harness-claude`) runs end-to-end on real Firecracker microVMs on the dev VM: per-run checkpoint pushes via `git push`, hot-suspend → `Idle` → auto-resume on next prompt all working; `engram session prompt` drives a real `claude --resume <id>` round-trip across the snapshot/restore boundary.

### Phase 4.5 — Apple Silicon backend ✅

**Goal**: real microVM isolation for Mac contributors, exercising the same in-VM code paths as Firecracker locally instead of round-tripping every test through a remote Linux dev VM. ADR 0003 captures the design decisions.

- ✅ `engram-sandbox-vz` — `SandboxBackend` impl driving Apple's Virtualization.framework directly from Rust via `objc2-virtualization`. No Swift driver. ~1500 LOC across `vm` (lifecycle wrapper), `console_bridge` (multi-port virtio-console host↔UDS pump), `disk` (APFS clone helper), `snapshot` (clone-based manifest), `backend` (the trait impl).
- ✅ **Multi-port virtio-console for the host↔guest control plane** — universally supported by Linux kernels (no `CONFIG_VIRTIO_VSOCKETS=y` requirement). Three logical ports: 1024 = agentd, 1025 = bootstrap, 1026 = harness adapter. The host fd-pair for each port is given to us at VM-config time via `VZFileHandleSerialPortAttachment`, exposed as `tokio::AsyncFd` over `libc::open(O_NONBLOCK)`.
- ✅ **`engram-transport` — backend-agnostic transport trait** — `Transport::{dial, listen}` trait with `VsockTransport` (Linux only, Firecracker) and `ConsoleTransport` (Linux only, VZ). All four in-VM binaries (`agentd`, `bootstrap`, `harness-noop`, `harness-claude`) dispatch on `ENGRAM_TRANSPORT=vsock|console` at runtime. Image baker injects the right value into the init shim at bake time.
- ✅ **APFS-clone-based snapshots** replace VZ's `saveMachineStateToURL`/`restoreMachineStateFromURL` — that pair is broken upstream for arm64 Linux guests on macOS (UTM #6654, Apple DevForum 745168, and Apple's own `containerization` framework avoids the API for the same reason). Snapshot pauses the VM, APFS-clones the per-sandbox rootfs into the snapshot dir, resumes — the clone *is* the snapshot. Restore clones it back into a fresh per-sandbox rootfs and cold-boots a new VM. `engram-bootstrap` supervisor + `claude --resume <id>` carry conversation continuity across the cold boot. APFS `clonefile(2)` runs in ~50 ms even on a 1.7 GB ext4 rootfs.
- ✅ **Per-sandbox rootfs** — each sandbox gets its own `<work_dir>/<sandbox_id>.rootfs.ext4` (cloned from the bake's warm-1 image at `create()`). Concurrent sandboxes no longer share a writable disk. Required for clone-based snapshots to be safe; was a latent correctness bug masked by the previous shared-rootfs setup.
- ✅ **Kata Containers static kernel** — Linux 6.12.28 with a VZ-tuned kconfig (PCI/ACPI/USB/sound/graphics stripped, VIRTIO_BLK/NET/CONSOLE built in). Same kernel `apple/container` uses by default. `just vz-pull-kernel` fetches it.
- ✅ **Sub-second timings**:
  - Cold boot (clone bake rootfs → VM up → agentd listening): **562 ms**
  - Cold resume (clone snapshot rootfs → fresh VM → bootstrap supervised): **746 ms**
  - Warm-pool checkout: **30 ms** (same `PooledBackend` wrapper as FC)
- ✅ **Codesign + CI** — `just vz-codesign` ad-hoc-signs binaries with `com.apple.security.virtualization`; `.github/workflows/ci-macos-vz.yml` runs the unit tests on `macos-15` arm64. 14 VZ unit tests pass.
- ✅ **End-to-end demo verified** — `docs/demo-vz.md`. Full lifecycle on macOS Apple Silicon: `status_changed → run_started → harness_idle → snapshot_taken → evicted → idle → resumed → active → run_started`.

**Not solved (intentional):**

- **No hot resume on macOS.** VZ's broken save/restore means every resume is a cold boot from the cloned rootfs. Sub-second is fine for Mac dev; production needs FC's UFFD-backed hot resume which only works on Linux + KVM.
- **No production deployment story for VZ.** It's a dev backend. Mac Studio / Mac mini fleets are interesting in principle but we're not committing to that path yet.

### Phase 5 — Image baking pipeline

**Goal**: warm-pool images refresh on a schedule, no human in the loop. Composes with Phase 4's git-native sessions: each warm image includes a clone of the writable repo at "bake SHA" + dependencies + warm daemons, so session create / cold resume runs `git fetch + reset` (milliseconds) instead of a full clone (seconds-to-minutes).

The baker itself is real today (`crates/engram-image-builder`):

- ✅ One-shot bake from a `Dockerfile` + `engram.toml`: `docker build` → `docker create + export | tar -x` → manifest.toml → optional `mke2fs -t ext4 -F -d` packing
- ✅ `Format::{Directory, Ext4}` — Directory for the dev backend, Ext4 for Firecracker
- ✅ `AgentInjection` — bakes the static-musl `engram-agentd` to `/sbin/engram-agentd` + writes `/sbin/engram-init` with the negotiated vsock port substituted in
- ✅ `image_versions` row recorded in Postgres via `record_in_metadata`
- ✅ CLI surface: `engram image build --repo <r> --source . [--format directory|ext4]`

What Phase 5 still needs to ship:

- **Cron-driven schedule** — k8s CronJob / systemd timer / GitHub Action; the baker is invoked manually today
- **`git clone <repo>` step inside the bake** — currently the `--source` is a local path; production wants the baker to clone fresh from the remote, using a GitHub App token from `SecretStore` with fine-grained `contents:read` scope for that repo
- **Repo setup-script execution** — running an `engram.toml`-declared `setup = ["pnpm install", "cargo fetch"]` inside the build sandbox before snapshotting (the slow stuff that benefits most from warm-pool caching)
- **Image registry as a Docker registry** — for Firecracker production, images live in a standard OCI registry (Artifact Registry, ECR, GHCR, self-hosted Distribution); host-agents pull on demand via the standard registry API. Phase 4's blob-storage removal means image distribution stops trying to be a bespoke subsystem and reuses tooling people already have. Engram-baked images are still ext4 rootfs + manifest.toml; the registry just transports them
- **Warm-pool refresh on new image version** — existing warm VMs drain naturally on checkout; replenishes spawn from the latest tag. No mid-flight migration
- **Image GC** — old image versions retire after a configurable TTL (default 24h after last access); blobs lifecycle to coldline; eventually deleted

**Deliverable**: schedule the baker every 30 min in CI. Observe new image versions appear in `image_versions` with `status=ready`, and warm-pool VMs spawn from the latest tag without disrupting in-flight sessions.

### Phase 6 — Production operability

**Goal**: cross-cutting concerns that make the system runnable. Some pieces land earlier as adjacent phases need them.

- **Auth**
  - Client tokens (`Authorization: Bearer ...`) with scopes per session/repo
  - Host-agent: pre-shared token (simple), mTLS (multi-tenant), or cloud workload identity (JWT from IMDSv2)
  - Per-session signed URLs for `/events` SSE so a Slack thread URL can't be replayed off-thread
  - **Real GCP Secret Manager backend** — `engram-secrets-gcp` is a typed stub today (`AccessSecretVersion` not yet wired). Phase 1's `Literal` and `Broker` modes work against the env-backed `engram-secrets-dev` impl; Phase 6 lights up the cloud-backed one.
  - **Broker-mode HTTPS proxy** — secret value substitution at the network boundary (coordinator-allocated proxy per session, intercepts outbound HTTPS to `allow_hosts`, swaps placeholders for real values). Today `secret_mode = "broker"` results in placeholders that don't authenticate anything; the broker proxy is what makes Broker mode functional.
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
- **TLS** for the WS channel, the HTTP API, and outbound calls to SecretStore / image registry
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
  - `engram session list` — running sessions with status, host, last activity ✅ (Phase 1)
  - `engram session logs <id>` — tail / replay events ✅ (Phase 1)
  - `engram host list/get/drain` — operator drain ✅ (Phase 3)
  - `engram image build <repo>` — trigger a baker run ✅ (Phase 1)
  - **`engram image list <repo>`** — list registry tags. CLI subcommand exists today but errors with `NotImplemented` because the coordinator doesn't expose a `GET /images/:repo` endpoint yet. Lands here.
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

### ~~Risk 4 — In-guest agent (closed)~~

**Closed.** `engram-agentd` shipped in Phase 2: length-prefixed bincode over vsock (host CONNECT-handshakes through Firecracker's UDS proxy), exec verb only, no auth yet. Static-musl binary baked into the rootfs by `engram-image-builder`'s `AgentInjection` path. Auth + the broader verb set (`Stat`/`Upload`/`Download`/`Ping`/`Shutdown`) land in Phase 6.

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

### Two layers of secrets: image vs harness

Secret responsibility is split along the same axis as everything else (workspace runtime vs agent runtime):

- **Image-level secrets** — declared via `[secrets.X]` in the manifest. Things the *workspace* needs: `NPM_TOKEN` for `npm install`, `GITHUB_TOKEN` for `git push`, project-specific build credentials. Schema travels with the image because the secret is intrinsic to running that image's tooling, regardless of who's driving.
- **Harness-level secrets** — credentials the *agent runtime* needs: `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` for the `claude` harness, future `OPENAI_API_KEY` for a hypothetical `codex` harness. These do **not** belong on the image — the same image hosts whatever harness the operator picks at session-create time, and a harness's auth requirement shouldn't bleed into the image's identity.

Today the harness-level layer is implemented as a **hard-coded UX special case in the dashboard**: when the user picks `harness=claude`, the form shows an OAuth-vs-API-key dropdown and one masked input. Whichever the user picks gets sent under the matching env-var name (`CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`); the coord folds it into the harness's env literally. Image-level credentials still render their own panel from `manifest.required_secrets`.

The deliberate v1 cutoff is "no harness-pack credential schema." Long-term: each harness pack ships a `pack.toml` declaring its own `[[secret]]` entries (name, allow_hosts, description), `/api/harnesses` returns it alongside name/description, and the form derives the UI instead of hard-coding `claude`. That's the natural finish but doesn't unblock anything in v1, so it stays deferred.

### Secret modes

`manifest.secret_mode` picks how resolved values reach the guest:

- **`literal`** — real values land as plain env vars. Simple, fast, dev only. Trivial to set up; secrets are visible to anyone with code execution inside the sandbox. Per-request overrides on `POST /sessions { secrets: {…} }` are honoured here (the dashboard's user-pasted values flow through verbatim, and unknown-name overrides are folded into the env directly — they don't have to appear in `manifest.secrets`).
- **`broker`** — random placeholders (`engram_ph_<sessionId>_<hash>`) land as env vars; a per-session network proxy MitMs outbound HTTPS, terminates TLS using a hostname-derived spoofed leaf cert signed by a per-deployment managed CA, scans `Authorization` / URL params / request bodies for the placeholders, and swaps the real value back in only on calls whose host matches `schema.allow_hosts` / `schema.allow_host_patterns`. The agent process never sees the real credential, so prompt-injection exfiltration attacks fail. Modeled on microsandbox's `Secret.env(..., allow_hosts=...)` design.

The proxy that performs broker-mode substitution is **not yet implemented** — `secret_mode = "broker"` results in unsubstituted placeholders today. Production rollout for Cortex is gated on this. The implementation is sized as its own focused work (~1500 LOC + cert-management plumbing) and tracked under Phase 2's deferred list with the implementation sketch; a half-measure HTTP-only proxy that delivers `allow_hosts` enforcement without actual substitution was considered and rejected — it doesn't deliver what the docstring promises and would need to be replaced wholesale once the real implementation lands.

### Status of the secret pipeline (v1)

Tracking the gap between the design above and what's wired today, so it's explicit what production hardening still owes:

| Piece | Status | What's missing |
|---|---|---|
| `[secrets.X]` schema parse + validation | ✅ wired | — |
| `SecretStore` trait + env / dotenv impls | ✅ wired | — |
| `Literal` mode injection | ✅ wired | — |
| Per-request `secrets: {…}` overrides | ✅ wired | — |
| `Broker` mode placeholder generation | ✅ wired | — |
| Broker-mode HTTPS proxy + cert injection | ❌ not wired | The whole substitution pipeline (Phase 6 in the verification matrix) |
| Real secret-manager backend | ❌ stub | `engram-secrets-gcp` is a typed placeholder; `AccessSecretVersion` not yet wired |
| Harness-pack credential schema | ❌ deferred | `pack.toml` + `/api/harnesses` enrichment; today the dashboard hard-codes `claude` |
| Per-deployment managed CA cert | ❌ not wired | Bake-time injection into rootfs trust store |

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
| 1 ✅ | `just dev` (subprocess backend on any platform), `curl -X POST :8090/sessions -d '{"repo":"local://demo"}'`, exec a command, verify output. Cold start <5s. |
| 2 ✅ | On a Linux + KVM host: `just dev-firecracker`, repeat Phase 1 verification, then idle a session, observe FC snapshot, evict via `DELETE /sessions/:id/local`, resume in <100ms via UFFD. The 5-test integration suite (`crates/engram-sandbox-firecracker/tests/`) drives all of this against real microVMs. |
| 3 ✅ | Stand up 2 hosts. Spawn sessions, verify even distribution per `engram host list`. `kill -9` one host, observe sessions transition `Active → Dead` within 30s via the dead-host detector. Restart a coordinator replica; SSE streams reconnect via `Last-Event-ID`. |
| 4 ✅ | Same orchestration runs against the same coord backend whether the host is FC, VZ, or process. Per-run git checkpoint pushes; idle-evict + auto-resume; `engram session fork` from a Dead session. ADRs 0001 + 0002. |
| 4.5 ✅ | macOS Apple Silicon: `just vz-pull-kernel && just vz-bake-demo && just dev-vz`. Cold boot <1s; full lifecycle (`harness_idle → snapshot_taken → evicted → idle → resumed → active`) end-to-end. ADR 0003. |
| 5 | Watch image-builder cron run, verify new image appears in `image_versions` with `status=ready`, observe warm pool VMs spawning from new tag without disrupting in-flight sessions. |
| 6 | Run on GCE Spot. Trigger preemption via `gcloud compute instances simulate-maintenance-event`, verify the host-agent's preemption handler fires `checkpoint_session` for each live sandbox before the VM dies. Sessions land `Dead`; calling system forks the workspace to continue. Production hardening (TLS, auth, broker proxy, jailer) all green. |
| 7 | Load test with locust/k6: 100 concurrent sessions. Chaos test: kill coordinator, kill hosts, kill Postgres briefly. Web app + Slack bot consume `/events` SSE in real time. |

End-to-end smoke test for v1 (Phase 1+2):
```bash
# On a GCE n2d-highmem-32 with nested virt enabled, a Hetzner box, or an Apple Silicon Mac for local dev
git clone https://github.com/cortex/engram && cd engram
cp .env.example .env  # fill in Postgres URL, GCS creds
docker-compose up -d
sleep 10

# Spawn a session
SID=$(curl -s -X POST localhost:8090/sessions \
  -d '{"repo":"local://hello-world","branch":"main"}' | jq -r .session_id)

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

For Phase 2 the equivalent end-to-end is the FC integration suite (each test is `#[ignore]`'d and runs on a Linux + KVM host):

```bash
bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh all
# boot, lifecycle, snapshot (file-backed), snapshot_uffd (lazy paging),
# exec_real_vm (bake → boot → vsock CONNECT → exec → assert stdout)
```

Phase 1 + 2 are landed against the dev VM. The remaining items in the Phase 2 list above are scoped under Phase 4 (cloud / spot) and Phase 6 (production hardening).

---

## Critical files (current)

The pieces that load-bear the Phase 1+2 surface, for new contributors finding their way around:

- `crates/engram-core/src/traits/{cloud,storage,metadata,sandbox,secrets}.rs` — pluggable seams
- `crates/engram-core/src/types/{ids,sandbox,session,snapshot,event,host,image}.rs` — shared types
- `crates/engram-core/src/error.rs` — `SandboxError` / `MetaError` / `StorageError` / `BackendError`
- `crates/engram-coordinator/src/api/{sessions,exec,events,snapshot,auth,health}.rs` — HTTP surface
- `crates/engram-coordinator/src/state.rs` — `SessionEventBus`, `SandboxRegistry`, `AppState`
- `crates/engram-host-agent/src/{pool,snapshot}.rs` — warm pool, snapshot manager scaffold
- `crates/engram-sandbox-firecracker/src/{lib,client}.rs` — FC HTTP-over-UDS client + backend impl
- `crates/engram-agentd/src/{proto,handler,main}.rs` — in-guest agent + wire protocol
- `crates/engram-uffd-handler/src/{proto,runtime,main}.rs` — UFFD page-fault handler
- `crates/engram-image-builder/src/{lib,docker,ext4}.rs` — bake pipeline
- `crates/engram-{secrets-dev,secrets-gcp}/src/lib.rs` — `SecretStore` impls
- `deploy/migrations/{0001_initial,0002_session_events}.sql`
- `flake.nix`, `rust-toolchain.toml` — pinned dev toolchain

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
