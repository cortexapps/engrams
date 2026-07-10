# Engram — Design

A self-hosted, open-source orchestrator for ephemeral AI agent sandboxes. Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: chunked-OCI rootfs + canonical-memory restore (sub-second cold start without pre-warming), FC snapshot lifecycle (UFFD-backed hot resume), multi-host scheduling, chunked-immutable content-addressed durability, and a pluggable cloud abstraction. A subprocess-based dev backend lets the entire orchestration layer run on macOS for fastest-possible iteration; an Apple Silicon backend drives Apple's Virtualization.framework for real microVM isolation locally. Production isolation is always Firecracker.

> This file is the **design** layer — architecture, traits, components,
> rationale. The chronological "what shipped when" log lives in
> [`docs/history.md`](./docs/history.md). The live punch list (what's
> still in flight + what's deferred) lives in
> [`docs/chunked-storage-rollout.md`](./docs/chunked-storage-rollout.md).
> The operational guide for GCP/GKE is [`docs/deploy.md`](./docs/deploy.md).
>
> ADRs in `docs/adr/` capture non-obvious decisions. The current
> trajectory:
>
> - **ADR 0001** — sessions are versioned conversations (workspace +
>   transcript externalized to git + Postgres). *Superseded by ADR 0005.*
> - **ADR 0002** — engram is a one-shot task runner (no cross-host cold
>   resume; sessions live ↔ FC snapshot lifetime). *Amended by ADR 0005.*
> - **ADR 0003** — Apple Silicon backend via Virtualization.framework
>   (APFS-clone snapshots, virtio-console transport, sub-second cold
>   boot).
> - **ADR 0004** — registry-backed image and harness distribution (OCI
>   artifacts, content-addressable host-side cache, KEK-sealed registry
>   credentials).
> - **ADR 0005** — disk-pressure blob tier reintroduced; git removed from
>   the platform layer. *Two-tier framing superseded by ADR 0007.*
> - **ADR 0006** — egress proxy lives on each FC host-agent (not the
>   coordinator). Deployment-wide CA loaded via a pluggable `CaSource`
>   trait; per-session policy ships from the coordinator over the
>   existing WS as a `NotifyKind::SessionEgressPolicy` frame.
> - **ADR 0007** — chunked-immutable content-addressed storage replaces
>   the tar+zstd cold-tier flush. Disk + memory state lives as
>   sha256-keyed chunks in `BlobStorage`; manifests are versioned
>   references. Sessions live ↔ chunks reachable; no hot/cold
>   dichotomy. Hardware-enforced memory COW across sessions sharing an
>   image. **This is the current durability primitive.**
>
> **Sections below that describe the two-tier (hot/cold) snapshot
> model reflect ADR 0005 as a historical record.** ADR 0007's
> chunked-immutable storage rolled out through Phase 7: the cold-tier
> flush pipeline + sealed-blob columns + `SessionStatus::ColdEvicted`
> + `SnapshotResidency` enum + envelope-encryption quartet on
> snapshots are all deleted (migration 0020). Snapshots reference
> chunks in `BlobStorage` via `disk_manifest_id` +
> `memory_manifest_id`; sessions live ↔ chunks reachable. The
> architectural description below predates the deletion — read it
> for design intent + the ADR 0001/0002/0005 trajectory, but
> cross-check against the current code. The README's "Architecture"
> section is the current source of truth for the live system shape.

---

---

## Context

We want a Modal-style sandbox-as-a-service for AI coding agents (think Stripe's Minions, Ramp's Inspect) — but **open-source and self-hostable**, so any organization can run their own without vendor lock-in.

The unsolved gap: existing open-source primitives (Firecracker, Cloud Hypervisor, libkrun, E2B's infra repo) give you per-VM mechanics. Nobody ships the **orchestration layer above the VM** in a clean, portable way: chunked-OCI image distribution, snapshot tiering, multi-host scheduling, cloud-backend abstraction, spot/preemptible eviction handling. Engram fills that gap, on top of Firecracker.

Cortex (our org) runs on GCP. Other adopters will run on AWS, Hetzner, k8s, or bare metal. Engram is built **GCP-first but cloud-agnostic**: all cloud-specific surfaces live behind traits with stub implementations for non-GCP backends shipped from day one.

The architecture comes out of an extended design discussion that explored: Stripe Minions / Ramp Inspect / Modal mechanics, single-box vs multi-host scaling, snapshot lifecycle, K8s vs raw VMs, spot tolerance, and graceful failure. This document captures the conclusions.

---

## Goals

1. **Sub-second sandbox spawn** — chunked-OCI rootfs + canonical-memory restore (ADR 0008) get cold start to a few seconds; the warm-pool tier on top (ADR 0014 M1, restored after the Phase-8 retirement) keeps the lease path on a pre-restored microVM at p99 ≤ 250 ms. The portable-snapshot primitive doubles as the substrate for cross-host durable resume (ADR 0014 M2).
2. **Snapshot-evict mechanic** for time-sharing host RAM across more sessions than fit at once.
3. **Pluggable cloud backend** so the project ports cleanly between GCP, AWS, Hetzner, and self-hosted bare metal.
4. **Pluggable storage backend** (GCS, S3, MinIO, local) for snapshot durability.
5. **Spot/preemptible-tolerant** by default — eviction = forced snapshot + resume elsewhere, not data loss.
6. **Recoverable from host loss** without losing user work — the cold tier (`BlobStorage`) survives host loss, disk-pressure flushing, and operator drains (ADR 0005). Sessions die only when both snapshot tiers are gone.
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
         │chunked  │         │chunked  │         │chunked  │
         │-OCI cache│        │-OCI cache│        │-OCI cache│
         │harness  │         │harness  │         │harness  │
         │  hub    │         │  hub    │         │  hub    │
         └────┬────┘         └────┬────┘         └────┬────┘
              │ cold-tier flush (tar+zstd, KEK-sealed URL)
              ▼
       ┌─────────────────────┐         ┌─────────────────────┐
       │  BlobStorage        │         │  Postgres (managed) │
       │  (GCS / S3 / local) │         │   - sessions        │
       │   - snapshot blobs  │         │   - session_events  │
       │   - tar+zstd FC dir │         │   - snapshots       │
       │   - sealed URL refs │         │   - hosts           │
       └─────────────────────┘         └─────────────────────┘
```

Snapshot store has **two tiers** (ADR 0005). **Hot tier** lives on each host's local NVMe — an FC `memory.bin` + state file for sub-100ms same-host resume, an APFS rootfs clone for VZ. **Cold tier** is the same payload tar+zstd-compressed and pushed to a `BlobStorage` backend; the URL is KEK-sealed and persisted on the snapshot row. Disk-pressure or admin flush moves a session from hot to cold; a cold-tier resume on any host with capacity materialises a fresh local copy and continues. Sessions go `Dead` only when *both* tiers are gone.

### Component summary

- **`engram-coordinator`**: stateless HTTP service (axum). Owns scheduling, host registry, session metadata, idle evictor, persistent SSE event bus. Backed by Postgres. Multiple replicas re-broadcast events to each other via `LISTEN/NOTIFY` on `session_events` + `host_dead`. `--mode=all` registers a local backend in-process for single-binary dev.
- **`engram-host-agent`**: per-host daemon. Composes a local `SandboxBackend` (the VMM driver — FC/VZ/Process) and a local `HarnessHub` (in-VM adapter routing) into a `LocalHostClient` that the coordinator talks to over WS via the matching `RemoteHostClient`. The `HostClient` trait (`engram-core::traits::host_client`) is the coord↔host boundary; `SandboxBackend` is strictly the local VMM seam. The split lands in ADR 0011 — before it, `SandboxBackend` was tangled with the wire surface, with `RemoteSandboxBackend` faking host-local methods (`snapshot_path_for`, harness routing) that don't have meaningful answers across a network. Three VMM impls ship today:
  - `engram-sandbox-firecracker` — Linux + KVM. Production. Firecracker's HTTP-over-Unix-socket API + UFFD-backed memory restore.
  - `engram-sandbox-vz` — macOS Apple Silicon. Apple Virtualization.framework via `objc2-virtualization` bindings. APFS clone-based snapshots. Mac dev with real microVM isolation. (ADR 0003.)
  - `engram-sandbox-process` — anywhere. Subprocesses, no isolation. Fastest iteration loop for orchestration-layer work.
- Maintains the chunked-OCI image cache + tiered chunk resolver (host-side `PooledBackend` wrapper, agnostic of which `SandboxBackend` is wrapped), `HarnessHub` TCP listener for in-VM harness adapters dialing back, preemption signal handler. Heartbeats `(capacity, utilization, draining)` to the coordinator.
- **`engram-agentd`**: in-VM exec daemon + harness supervisor (PID 1 after the init shim). Length-prefixed bincode over the configured transport. Verbs: `Exec` (streaming), `Stat`, `Upload`, `Download`, `StartShell`, `Ping`, `Shutdown`, `SpawnHarness`. First-frame token handshake (server side) gates non-trivial verbs. Owns the harness child process; each `SpawnHarness` kills the previous child and exec's a fresh one — clean re-spawn point on resume. On startup, dials the host's per-sandbox ready UDS so the host can block on `accept()` rather than poll for "is the in-VM listener bound."
- **`engram-transport`**: backend-agnostic transport trait. `VsockTransport` (FC) and `ConsoleTransport` (VZ); chosen at runtime via `ENGRAM_TRANSPORT` set by the stage-1 init shim (injected by the materializer, ADR 0080).
- **`engram-rootfs-materializer`**: host-side OCI→rootfs materializer (ADR 0080). Pulls a standard OCI/Docker image, whiteout-aware-flattens the layers, injects the stage-1 `/sbin/engram-init` shim (the only engrams-owned file baked in), packs a deterministic ext4 via `mke2fs`, and chunks it into `BlobStorage`. Driven by the `MaterializeImage` host RPC at enable/rebase time — agentd, the harness, and ttyd are *not* injected; they ride host-staged bundle slots.
- **`engrams` CLI** (`cli/`, Bun/TS — not a crate): the product CLI. Talks to the ORCHESTRATOR's Connect surface (`engrams task|session|image|registry|host|profile|apikey|admin …`); auth via `engrams auth login` (device flow → user-owned API key) or `ENGRAMS_API_KEY`. The coordinator app-gRPC is internal (orchestrator-only).

---

## Source-of-truth model

Four layers of state, with explicit durability guarantees (ADR 0005 supersedes the git-as-durability framing of ADR 0001):

| Layer | Contents | Durability | On loss |
|---|---|---|---|
| **Postgres** | session metadata, `session_events` (conversation log + tool calls), host registry, sealed cold-blob refs | permanent (managed/backups) | hard failure — must guard |
| **Image registry** | plain OCI session images (ADR 0080) + harness / agentd / guest-tools bundles; the materializer pulls the image at enable time, host-agents stage the bundles | reproducible from Dockerfiles + push pipelines | rebuild |
| **Hot snapshot store** | per-host local NVMe — FC `memory.bin` + state file on Linux, APFS rootfs clones on VZ. Sub-second resume on the same host. | host-local only; not replicated | falls back to cold tier if `blob_present`, otherwise the session goes `Dead` |
| **Cold snapshot store** | tar+zstd of the FC snapshot dir, stored in a `BlobStorage` backend (S3/GCS/local fs); sealed blob URL persisted in Postgres under the deployment KEK | permanent (replicated by the bucket / backed up by the deployment's own policy) | session goes `Dead` (terminal) |

**Design rule**: snapshots come in two tiers, with the cold tier as the cross-host durability primitive. Sessions live ↔ snapshot residency in *either* tier. Hot is the fast path; cold survives host loss, disk-pressure flushing, and operator-initiated drains. The conversation log in Postgres survives independently — historical events are queryable forever — but the agent's in-memory state lives or dies with the snapshot pair.

Git is **not** in this table. Agents that want their work to land in a remote do `git push` themselves inside the sandbox using credentials mounted via `[secrets.GITHUB_TOKEN]` (or SSH key); that's a tool the agent uses, not a layer the platform owns.

---

## Components — detailed

### Coordinator (`engram-coordinator`)

**Responsibilities:**
- Receive session requests (`POST /sessions`). Pick a host, return session id + persistent SSE event stream.
- Maintain host registry. Heartbeats from each host every 5s: capacity (vCPU / memory / disk), local snapshots held, draining flag.
- Track session ↔ host ↔ sandbox routing in Postgres so coordinator restart restores active sessions.
- Drive scheduling decisions: prefer host with a warm slot for the session's template (ADR 0014 M1) → host with snapshot local → host with largest free capacity → fail. (Phase 8 / ADR 0008 deleted the original warm pool because its key was leaking abstractions; ADR 0014 restores the tier on top of portable snapshots, keyed on `template_ref` — a content-addressed handle that side-steps the prior correctness sharp edges.)
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
- Host-side `PooledBackend` wrapper composes the chunked-OCI image cache, tiered chunk resolver, egress proxy, and NBD pool onto any `SandboxBackend` (FC / VZ / process). Each session-create resolves the image's rootfs through the cache (NVMe → BlobStorage → OCI registry; ADR 0008) before delegating to the underlying backend. The `PooledBackend` is what the host-agent's `LocalHostClient` wraps as its `SandboxBackend` inner; `LocalHostClient` itself adds harness routing (`bind_session`/`unbind_session`/`send_prompt`) by referencing a shared `HarnessHub` (ADR 0011).
- Drive sandbox lifecycle through `SandboxBackend` (`create`/`destroy`/`exec_stream`/`snapshot`/`restore`/`start_agent`/`set_harness_sink`); harness routing through `HarnessHub` (`bind_session`/`unbind_session`/`send_prompt`/`accept_via_session_lookup`). The hub's `EventSink` ships every per-session event over the WS as `NotifyKind::HarnessEvent`; the coord's read loop re-emits those into its in-proc hub so SSE subscribers see the same stream as mode=all.
- Run snapshot manager (host-local; see below).
- Subscribe to `cloud.preemption_signal()`; on notice fan out best-effort `checkpoint_session` to live sandboxes in parallel with a 25s deadline (Phase 4 Track D).
- Run the `HarnessHub` TCP listener: in-VM harness adapters dial back via `engram-transport` (vsock or virtio-console) → host-side TCP forwarding → `session_events` ingestion.
- Enforce per-VM resource limits via the production backend (Firecracker enforces RAM/CPU/disk at the VMM boundary; the dev backend ignores them with a documented caveat).
- Heartbeat coordinator with capacity + local snapshots.

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

### Rootfs materializer (`engram-rootfs-materializer`)

ADR 0080 retired the `engram-image-builder` bake path: a session image is now a plain `docker build && docker push` of a standard OCI image, and the OCI→rootfs conversion moved host-side, off the per-session path.

- Runs at **enable/rebase time** as the `MaterializeImage` host RPC (a sibling of `build_base_snapshot`), never per-session — so per-session latency is zero.
- Pulls the standard OCI/Docker image (platform by host arch), whiteout-aware-flattens the layers (xattrs / hardlinks / setuid / ownership, `FollowSymlinkInScope` semantics), extracts the OCI config blob → `OciRuntimeDefaults` (Dockerfile `ENV`/`WORKDIR`), injects the stage-1 `/sbin/engram-init` shim, packs a deterministic ext4 via the pinned `mke2fs`, and chunks the result into the host chunk store / `BlobStorage`.
- Host safeguards keep host disk bounded: ADR 0078 disk-veto host pick, a ~2.5× image-size scratch budget integrated with chunk-cache accounting, scrub on all exit paths, an enable-time image-size cap, and ≤1 concurrent materialize per host.
- The only engrams-owned file it bakes into the rootfs is the init shim; agentd, the harness, and ttyd ride host-staged bundle slots and are swapped in per-session (zero image re-bakes when they change).

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

Cold-tier durability primitive — ADR 0005 reintroduced it after ADR 0001 had retired it. The original framing ("30-second-preemption replication is the design point") was infeasible math; the new framing ("minutes-budget disk-pressure flush") is trivial math, and the trait carries its own weight.

```rust
#[async_trait]
pub trait BlobStorage: Send + Sync {
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError>;
    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError>;
    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError>;
    async fn delete(&self, key: &str) -> Result<(), BlobError>;
}
```

**v1 implementations:**
- `engram-storage-gcs` — Google Cloud Storage via `google-cloud-storage` v0.24. ADC + Workload Identity in production, `STORAGE_EMULATOR_HOST` for fake-gcs-server in `just dev`. `put_streaming` forwards the body through `upload_streamed_object` so multi-GB FC `memory.bin` flushes don't materialise in host-agent RAM.
- `engram-storage-s3` — S3 via the AWS SDK. Same surface, different backend.
- `engram-storage-local` — filesystem-backed `<local_path>/blobs/`. Default in `just dev`; useful for single-host deployments that want cold-tier durability without standing up an object store.

The cold-tier flush pipeline (`engram-host-agent::flush::flush_session`) runs `sh -c 'tar -cf - -C <snapshot_path> . | zstd -3 -T0'` and pipes stdout to `BlobStorage::put_streaming`. The blob URL is sealed under the deployment KEK and stored on the `snapshots` row.

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

### `HostClient` (in `engram-core::traits::host_client`)

The coord↔host boundary trait. Two impls — one composes a local VMM (`LocalHostClient` in `engram-host-agent::host_client`, used in mode=all and inside the host-agent itself), one wraps a WS connection (`RemoteHostClient` in `engram-protocol::client`). `HostRegistry` on the coord side stores `Arc<dyn HostClient>` per host, dispatches by `sandbox_id → host_id`, and itself implements `HostClient` so the rest of the coord routes through one type. ADR 0011.

```rust
#[async_trait]
pub trait HostClient: Send + Sync {
    // sandbox lifecycle — delegates to the local SandboxBackend on the host
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;
    async fn exec_stream(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecStream, SandboxError>;
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;
    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError>;
    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError>;
    async fn guest_ip(&self, id: SandboxId) -> Option<String>;

    // harness routing — delegates to the host's local HarnessHub
    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId);
    async fn unbind_session(&self, session_id: SessionId);
    async fn send_prompt(&self, sandbox_id: SandboxId, text: String) -> Result<(), SandboxError>;

    fn harness_dial(&self) -> HarnessDial;
    fn set_harness_sink(&self, _sink: HarnessSink) {}
}
```

### `SandboxBackend` (in `engram-core::traits::sandbox`)

The VMM seam — *strictly* host-local. Production = Firecracker on Linux. Dev = subprocesses (anywhere) or Apple Virtualization.framework (Mac). Future VMMs (Cloud Hypervisor, raw libkrun, Kata) plug into the same trait if we ever need them. The coord doesn't import this trait at all; only the host-agent's `LocalHostClient` does.

```rust
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    async fn exec_stream(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecStream, SandboxError>;
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;
    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError>;
    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError>;
    async fn guest_ip(&self, id: SandboxId) -> Option<String>;
    fn snapshot_path_for(&self, id: SnapshotId) -> PathBuf;        // host-local file path
    fn set_harness_sink(&self, _sink: HarnessSink) {}              // closure: not crossable
    fn harness_dial(&self) -> HarnessDial { HarnessDial::Vsock }
}
```

**v1 implementations**:

- **`engram-sandbox-firecracker` (production, Linux)** — drives Firecracker over its HTTP-over-Unix-socket API. Each sandbox owns a Firecracker process, a per-VM vsock UDS, and (forthcoming) a TAP device. Snapshot via `PATCH /vm Paused` + `PUT /snapshot/create`. Restore via `PUT /snapshot/load` with either `backend_type=File` or `backend_type=Uffd` (lazy paging via the `engram-uffd-handler` companion process — sub-100ms resume regardless of guest RAM size). All `SandboxBackend` methods exercised by integration tests against real microVMs. Production hardening (jailer, TAP networking, broker-mode HTTPS proxy, `SendCtrlAltDel` graceful shutdown) lands in Phase 6.
- **`engram-sandbox-vz` (Mac dev, macOS Apple Silicon)** — drives Apple Virtualization.framework via `objc2-virtualization` bindings. Multi-port virtio-console for the host↔guest control plane (universal kernel support, no `CONFIG_VIRTIO_VSOCKETS=y` requirement). Snapshots are APFS rootfs clones (`clonefile(2)` ~50 ms even at 1.7 GB), since VZ's native `saveMachineStateToURL`/`restoreMachineStateFromURL` is broken upstream for arm64 Linux guests. Cold boot 562 ms; cold resume 746 ms. Same `engram-agentd` + harness binaries that run on FC. ADR 0003.
- **`engram-sandbox-process` (dev, anywhere)** — runs commands as plain host subprocesses, each rooted in a per-sandbox working directory. No isolation, no resource enforcement. Fastest-possible iteration loop for orchestration-layer work that doesn't need to exercise the in-VM code paths. `snapshot()` is a tarball of the workdir. **Never use in deployment.**

#### In-guest agent (`engram-agentd`)

Firecracker has no "exec a command in a running guest" primitive. Every guest runs `engram-agentd` (`crates/engram-agentd`) — a small Rust binary the stage-1 init copies out of its reserved bundle slot to `/run/engram/engram-agentd` and execs (ADR 0080; nothing engrams-owned is baked into the rootfs beyond the shim). It listens on AF_VSOCK port 1024 and proxies exec / stdin / stdout for the host agent. `SandboxBackend::exec_stream` on the Firecracker backend connects to Firecracker's vsock proxy at `<vsock_uds>`, performs the `CONNECT 1024\n` → `OK <peer_port>\n` handshake, sends a `WireExecRequest`, and streams `WireExecEvent`s back. Full design in the [In-guest agent](#in-guest-agent-engram-agentd) section below.

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

A small Rust binary every guest boots by exec'ing it out of the `agentd` bundle slot (ADR 0080) via the tiny baked init shim (`/sbin/engram-init`). Bridges the gap between Firecracker (which has no exec primitive) and the host agent. Lives at `crates/engram-agentd`.

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

`engram-agentd` ships as the fleet `bundle-agentd` (reserved slot `dyn_1`, ADR 0080); the only baked file is `/sbin/engram-init` (the stage-1 init shim that brings up kernel plumbing, mounts the bundle slots, copies agentd to tmpfs, and execs it), injected by the host-side `engram-rootfs-materializer` at enable/rebase time. agentd is built statically against musl so it runs in any base image regardless of the rootfs's libc / dynamic-linker layout — and an agentd change ships by republishing the bundle, with zero image re-bakes and zero base-snapshot recaptures (fresh restores re-exec onto the swapped generation).

Initial scope is exec-only — `Stat`/`Upload`/`Download`/`Ping`/`Shutdown` verbs from the original design are deferred. They'll land when the surface is needed (file upload for snapshot transfer, ping for liveness, etc.).

Versioning: TODO. Two consumers exist now (the agentd vsock proto + the Phase 3 coordinator-to-host WS proto in `engram-protocol`). Both are unversioned; first-frame version negotiation lands with the broader auth/handshake work in Phase 6.

---

## Database schema (Postgres)

The authoritative schema lives in `deploy/migrations/*.sql` — currently 17 sequential migrations applied by `sqlx::migrate!()` at coordinator startup. The original founding-design sketch reproduced here drifted heavily as the system grew (`messages` was replaced by `session_events`; `enabled_images`, `harness_packs`, `registry_credentials`, and `session_secrets` were added in Phase 5; the snapshot row gained the sealed-blob-ref quartet `(wrapped_dek, nonce, ciphertext, key_id)` in ADR 0005); rather than restate it in two places, treat the migrations directory as the source of truth.

**Key tables, briefly:**

- `sessions` — id, repo, image identity, current `host_id` + `sandbox_id` (NULL when no live VM), status (`Pending` / `Active` / `Idle` / `ColdEvicted` / `Dead` / `Failed`), timestamps.
- `hosts` — id, hostname, cloud metadata, capacity, heartbeat timestamp, status (`Ready` / `Draining` / `Dead`).
- `snapshots` — id, session, host, `local_path` (hot tier), `blob_present` + sealed-blob-ref columns (cold tier — ADR 0005), `image_version`, sizes, timestamps.
- `session_events` — append-only conversation + tool-call log keyed `(session_id, idx)`. Drives the SSE replay stream.
- `enabled_images` — per-deployment allowlist of OCI image URIs that sessions can spawn against (Phase 5b).
- `harness_packs` — `name → OCI URI` map for built-in harnesses (`claude`, `noop`).
- `registry_credentials` — KEK-sealed creds for OCI registry pulls. Encrypted with the same envelope shape as `session_secrets` (`engram-crypto::CredCipher`).
- `session_secrets` — sealed per-request overrides (e.g. `CLAUDE_CODE_OAUTH_TOKEN`) needed to rebuild the harness env on cold resume.

See `deploy/migrations/` for the live DDL.

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
    ├── engram-rootfs-materializer/    # host-side OCI image → flattened ext4 + chunks (ADR 0080)
    ├── engram-agentd/                 # binary + library: in-guest exec daemon + harness supervisor (transport-agnostic)
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

(`engram-storage-{gcs,s3,local}` were retired with ADR 0001 and brought back with ADR 0005; the cold tier is now the cross-host durability primitive.)

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

> **Moved to [`docs/history.md`](./docs/history.md).** The seven-phase
> rollout (orchestration layer → Firecracker → multi-host → harness
> protocol → Apple Silicon → registry → chunked storage + deploy)
> shipped between Q1 and Q2 2026 and now reads as a milestone log
> rather than active planning. Cross-reference the ADRs in
> `docs/adr/` for the design rationale at each pivot.
>
> The historical phase content below has been removed to keep this
> file focused on architecture; see the history doc for the
> chronological "what + when" view and the rollout doc for live
> punch-list status.

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

**Closed.** `engram-agentd` shipped in Phase 2: length-prefixed bincode over vsock (host CONNECT-handshakes through Firecracker's UDS proxy), exec verb only, no auth yet. It's a static-musl binary that rides the fleet `bundle-agentd` slot (ADR 0080); the stage-1 init execs it out of the bundle at boot, so nothing agentd-related is baked into the rootfs. Auth + the broader verb set (`Stat`/`Upload`/`Download`/`Ping`/`Shutdown`) land in Phase 6.

---

## Starter images & secrets

An "image" is a plain OCI image (`docker build && docker push`) plus its out-of-band **image-config** — name/description/env/workdir/resources/warm — supplied at enable time (ADR 0080). No secret values ever live in the image or its config.

### Source-side: a plain `Dockerfile`

ADR 0080: a session image is a standard OCI image, so a dev needs exactly one file in their repo — a `Dockerfile` — and builds + pushes it with stock tooling:

```
my-repo/
├── Dockerfile                # WHAT'S in the image — universal Docker syntax
└── ...source...
```

```bash
docker build -t registry.example.com/cortex/api:warm-1 .
docker push  registry.example.com/cortex/api:warm-1
```

`Dockerfile` is plain — no engram-specific syntax. Multi-stage builds, `FROM ... AS builder`, `COPY --from=builder ...`, BuildKit cache mounts, `--secret` build secrets — all supported, because it's just `docker build`. Devs reuse everything they already know about Dockerfiles.

The image contract is only **"any linux image with `/bin/sh`"**. `git` / `curl` / `socat` / `iproute2` are *workspace* requirements — the image installs them if the agent needs them (the demo image does) — not engrams requirements. ttyd is no longer needed in the image either: it comes from the host-staged `guest-tools` bundle (agentd falls back to a baked `/usr/local/bin/ttyd` only if the fleet stages no `guest-tools` bundle).

There is **no `engram.toml`** (retired with ADR 0080). Runtime config lives out-of-band in the enable-time image-config — see [Runtime config](#runtime-config) below.

### Why Dockerfiles, not a custom format

- **It's the universal "what's installed where" language.** Reinventing it would be ecosystem hostility.
- **Multi-stage builds, base-image reuse, build caching, secret mounts** are already solved by BuildKit. Free for us.
- **The OCI image is portable** — the same artifact runs as a container too (great for CI, debugging); the host-side materializer converts it to a chunked ext4 rootfs for Firecracker production (ADR 0080).
- **Convergence with the field** — Modal, AWS App Runner, Cloud Run, Fly Machines all work this way. There's a reason: container images are the unit of deployable code in 2026.

A small set of Dockerfile concepts don't translate cleanly to a microVM rootfs, and Engram maps them as follows:

| Dockerfile directive | In Engram |
|---|---|
| `FROM`, `RUN`, `COPY`, `ADD`, `ENV` | Honored as-is — they shape the rootfs. |
| `CMD`, `ENTRYPOINT` | **Ignored.** The kernel boots, init starts `engram-agentd`, agentd waits for exec RPCs. The user's `CMD` would be a process they start via exec, not the entrypoint. |
| `EXPOSE` | Informational only; networking is at the TAP boundary, not container ports. |
| `USER` | Honored. The agent runs as the image's `USER` if set. |
| `WORKDIR` | Honored as the default exec workdir. |
| `HEALTHCHECK` | Ignored; health is reported by `engram-agentd`. |

### Materialization (`engram-rootfs-materializer` + `MaterializeImage`)

There is no bake step and no `engram-image-builder` (ADR 0080). The OCI→rootfs conversion runs **host-side, at enable/rebase time** as the server-streaming `MaterializeImage` host RPC (a sibling of `build_base_snapshot`), off the per-session path:

1. **Pull** the standard OCI/Docker image, platform-selected by host arch.
2. **Flatten** the layers, whiteout-aware (`.wh.` / opaque dirs, xattrs / hardlinks / setuid, ownership; `FollowSymlinkInScope` symlink resolution).
3. **Extract** the OCI config blob → `OciRuntimeDefaults` (Dockerfile `ENV` / `WORKDIR`), merged under the admin image-config.
4. **Inject** the stage-1 `/sbin/engram-init` shim — the one engrams-owned file baked into the rootfs.
5. **Pack** a deterministic ext4 via the pinned `mke2fs` (clamped mtimes).
6. **Chunk** the result into the host chunk store / `BlobStorage`.

Host safeguards keep disk bounded: ADR 0078 disk-veto host pick, a ~2.5× image-size scratch budget, scrub on all exit paths, an enable-time size cap, and ≤1 concurrent materialize per host. Registry auth: static creds resolved coordinator-side and passed in the request; GCP workload identity resolved host-side. Per-session latency is zero.

agentd, the harness, and ttyd are **not** materialized into the rootfs — they ride host-staged bundle slots (`bundle-agentd` = `dyn_1`, `harness-claude` = `dyn_0`, `guest-tools` = `dyn_2`) and are swapped in per-session, so rolling any of them is zero image re-bakes.

### Docker dependency

Building + pushing the image requires a Docker-compatible runtime on the dev's / CI's machine (not on the engrams hosts — the materializer only *pulls* the pushed image). Verified compatible:

- **Docker Desktop** (Mac, Linux, Windows)
- **OrbStack** (Mac — fastest on Apple Silicon)
- **Colima** (Mac — VM-backed)
- **Podman** with `podman-docker` (Linux — daemonless option)
- **BuildKit standalone** via `buildctl` (production CI)

### Runtime config

Runtime config is supplied out-of-band at **enable time** via an image-config TOML (`ImageConfig`), never baked into the image (ADR 0080). It carries `name`, `description`, `env`, `workdir`, `resources`, and an optional `warm` section (the warm-capture `command` / `timeout_secs` / `workdir` / `env` / `network`):

```toml
# image-config.toml — passed to `engram image enable --config` / `image update --config`
name = "cortex-api"
description = "Backend API service"

[env]
PYTHONUNBUFFERED = "1"

[resources]
suggested_memory_mib = 4096
suggested_vcpus = 2
suggested_disk_gib = 20

# Optional warm-capture config (run once at enable/rebase time).
[warm]
command = "pnpm install"
timeout_secs = 300

  [warm.network]
  default = "deny"
  allow_hosts = ["registry.npmjs.org"]
```

The config is set via `engram image enable --uri <uri> --config <toml>` (required on first enable) and edited via `engram image update --uri <uri> --config <toml>`: cheap fields (name / description / env / workdir) apply immediately; a diff touching `resources` or anything under `warm` needs `--allow-recapture` and re-enqueues a capture job. Values persist in Postgres (`enabled_images.image_config`), merged with the image's `OciRuntimeDefaults` at session-create time. Per-image *secrets* and per-session *network* policy are session-policy concerns (ADR 0057), not image-config; harness credentials live one layer above the image (see below).

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
| 2 ✅ | On a Linux + KVM host: `just dev` (auto-detects KVM → Firecracker), repeat Phase 1 verification, then idle a session, observe FC snapshot, evict via `DELETE /sessions/:id/local`, resume in <100ms via UFFD. The 5-test integration suite (`crates/engram-sandbox-firecracker/tests/`) drives all of this against real microVMs. |
| 3 ✅ | Stand up 2 hosts. Spawn sessions, verify even distribution per `engram host list`. `kill -9` one host, observe sessions transition `Active → Dead` within 30s via the dead-host detector. Restart a coordinator replica; SSE streams reconnect via `Last-Event-ID`. |
| 4 ✅ | Same orchestration runs against the same coord backend whether the host is FC, VZ, or process. Per-run git checkpoint pushes; idle-evict + auto-resume; `engram session fork` from a Dead session. ADRs 0001 + 0002. |
| 4.5 ✅ | macOS Apple Silicon: `just pull-kernel && just bake-demo && just dev` (auto-detects VZ). Cold boot <1s; full lifecycle (`harness_idle → snapshot_taken → evicted → idle → resumed → active`) end-to-end. ADR 0003. |
| 5 | `docker build && docker push` a plain OCI image (ADR 0080), then `engram image enable --uri <uri> --config <toml>`; verify the enable job materializes the rootfs host-side and the image lands enabled, and that new sessions pick it up without disrupting in-flight sessions. |
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
# exec_real_vm (busybox-fixture rootfs → boot → vsock CONNECT → exec → assert stdout)
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
- `crates/engram-host-agent/src/{pooled_backend,snapshot}.rs` — chunked-OCI / image cache / egress composition; snapshot manager scaffold
- `crates/engram-sandbox-firecracker/src/{lib,client}.rs` — FC HTTP-over-UDS client + backend impl
- `crates/engram-agentd/src/{proto,handler,main}.rs` — in-guest agent + wire protocol
- `crates/engram-uffd-handler/src/{proto,runtime,main}.rs` — UFFD page-fault handler
- `crates/engram-rootfs-materializer/src/*.rs` — host-side OCI→ext4 materialize pipeline (ADR 0080)
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
- **Image version**: a plain OCI image tag (ADR 0080), materialized host-side into a chunked ext4 rootfs at enable/rebase time.
- **Coordinator**: stateless service that schedules sessions onto hosts.
- **Host agent**: per-VM-host daemon managing pool, snapshots, resources.
- **engram-agentd**: small in-guest daemon (Phase 2) that listens on vsock and proxies exec / stdin / stdout. Required because Firecracker has no native exec primitive.
- **Engram**: a stored memory trace. In our system: a persisted session state (snapshot + Postgres history).
