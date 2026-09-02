# Architecture

The component, storage, and wire detail that used to live in the README. `DESIGN.md` is the
design document; this file is the operational picture of what runs where. Decision records
are in `adr/`.

Three process classes, four storage primitives, three wire surfaces.

### Topology

```
              ┌───────────────────────────────────────────────────┐
              │                  engrams Coordinator               │
              │                  (axum, stateless, N replicas)    │
              │                                                   │
   HTTP +     │   - HTTP API: /sessions, /events (SSE), /admin    │
   SSE  ◄────►│   - Scheduler: snapshot affinity → capacity-fit   │
              │   - Idle evictor, dead-host detector,             │
              │     chunk-store GC scheduler                      │
              │   - Postgres LISTEN/NOTIFY for cross-replica      │
              │     event fan-out + dead-host coordination        │
              └─────────────┬─────────────────────────────────────┘
                            │ bincode-over-WS, WIRE_VERSION=1
                            │ (Frame { req_id, trace, kind })
                            │ Coord ↔ Host: hosts DIAL coord
                            │ (NAT-friendly; no inbound
                            │  to host required)
              ┌─────────────┼─────────────┬─────────────┐
              ▼             ▼             ▼             ▼
        ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐
        │ Host A  │   │ Host B  │   │ Host C  │   │  ...    │
        │         │   │         │   │         │   │         │
        │ engram- │   │ engram- │   │ engram- │   │         │
        │ host-   │   │ host-   │   │ host-   │   │         │
        │ agent   │   │ agent   │   │ agent   │   │         │
        │         │   │         │   │         │   │         │
        │ chunked │   │ chunked │   │ chunked │   │         │
        │ -OCI    │   │ -OCI    │   │ -OCI    │   │         │
        │ cache   │   │ cache   │   │ cache   │   │         │
        │  NBD    │   │  NBD    │   │  NBD    │   │         │
        │  daemon │   │  daemon │   │  daemon │   │         │
        │  (FC)   │   │  (FC)   │   │  (FC)   │   │         │
        │         │   │         │   │         │   │         │
        │  UFFD   │   │  UFFD   │   │  UFFD   │   │         │
        │  handlr │   │  handlr │   │  handlr │   │         │
        │  (FC)   │   │  (FC)   │   │  (FC)   │   │         │
        │         │   │         │   │         │   │         │
        │  micro  │   │  micro  │   │  micro  │   │         │
        │  VMs    │   │  VMs    │   │  VMs    │   │         │
        └────┬────┘   └────┬────┘   └────┬────┘   └─────────┘
             │             │             │
             │  reads/writes              writes
             │  chunks + manifests        snapshots
             ▼             ▼             ▼
        ┌──────────────────────┐    ┌─────────────────────┐
        │   BlobStorage        │    │  Postgres (managed) │
        │   (GCS / S3 / local) │    │   - sessions        │
        │                      │    │   - session_events  │
        │ - chunks/sha256/…    │    │     (conversation   │
        │   (immutable, 16 MiB │    │      log + SSE)     │
        │    disk / 512 KiB    │    │   - snapshots       │
        │    memory)           │    │     (manifest refs) │
        │ - manifests/<id>/    │    │   - hosts           │
        │   v<n>.json          │    │   - enabled_images  │
        │   (versioned, content│    │   - registry_creds  │
        │   -addressed)        │    │   - session_secrets │
        │ - traces/<id>/       │    └─────────────────────┘
        │   <host>.json        │
        │   (working-set       │    ┌─────────────────────┐
        │    prefault hints)   │    │  OCI Registry       │
        └──────────────────────┘    │  (Artifact Reg /    │
                                    │   ECR / Docker Hub) │
                                    │                     │
                                    │  - session images   │
                                    │    (plain OCI,      │
                                    │     docker build +  │
                                    │     docker push)    │
                                    │  - harness bundles  │
                                    └─────────────────────┘
```

### Process classes

**`engram-coordinator`** — stateless HTTP service (axum). Owns the public API, scheduling, idle eviction, dead-host detection, chunk-store GC, the persistent SSE event bus. Backed by Postgres. Multiple replicas behind a load balancer; replicas reconcile via `LISTEN/NOTIFY` on `session_events` + `host_dead` and race via `pg_try_advisory_lock` for exclusive-write operations (dead-host eviction). `--mode=all` registers a local host-agent in-process for single-binary `just dev`.

**`engram-host-agent`** — per-VM-host daemon. Dials the coordinator over WebSocket (NAT-friendly; coord never has to reach back). Composes a local `SandboxBackend` (FC / VZ / Process — the VMM driver) and a `HarnessHub` (in-VM adapter routing) into a `LocalHostClient`; that's what's served over the WS to the coord (ADR 0011 splits the trait surfaces: `SandboxBackend` = "what kind of VM," `HostClient` = "where the work happens"). Hosts the chunked-OCI image cache + tiered chunk resolver, the NBD daemon (chunked disks for FC), the chunk cache (NVMe-backed LRU), the materialize-dir orphan reaper, the filtering egress proxy on TCP/443 + UDP/53 + TCP/53 (ADRs 0006, 0010). Heartbeats `(capacity, utilization, draining, running_sandboxes)` every 5 s.

**In-guest binaries** (live inside each microVM; none are baked into the rootfs — the only engrams-owned file baked in is the stage-1 `/sbin/engram-init` shim, injected by the host-side rootfs materializer, ADR 0080):
- `engram-agentd` — PID 1's exec after the init shim (which copies it out of the fleet `bundle-agentd` slot to tmpfs, ADR 0080 — an agentd change ships by republishing the bundle, no image re-bakes). The in-VM control surface: serves length-prefixed bincode RPCs over the configured transport. Verbs: `Exec` (streaming), `Stat`, `Upload`, `Download`, `StartShell`, `Ping`, `Shutdown`, `SpawnHarness`. Owns the harness child process (kill+respawn on each fresh `SpawnHarness`, so the host has a clean re-spawn point on resume). On startup, dials the host on `ENGRAM_AGENTD_READY_PORT` so the host knows when the in-VM listener is bound — no boot-race polling on the host side.
- `engram-harness-{noop,claude}` — the agent runtime (Claude Code or a deterministic test harness), exec'd as agentd's harness child on `SpawnHarness`. Reports run state + tool calls back through the harness hub.

### Storage substrate (ADR 0007)

The durability primitive of engrams is **chunked-immutable content-addressed storage**. Disk + memory state lives as sha256-keyed chunks in any `BlobStorage` backend (GCS / S3 / local fs); manifests are versioned references that point at them.

| Tier | What | Where | Cost model |
|---|---|---|---|
| **Persistent** | sha256-keyed chunks (16 MiB disk, 512 KiB memory), versioned manifests, per-host working-set traces | `BlobStorage` impl (GCS / S3 / local) | dedup is automatic (content-addressed); the base image's GiBs are stored once regardless of session count |
| **Cache** | local NVMe chunk cache (LRU + pin-list + singleflight), per-host materialize-dir for assembled `.ext4` files | each host's `<work_dir>` | disk-derived absolute ceiling by default (ADR 0070: `min(60% of the disk, 80%)`, ~179 GiB on a 298 GiB disk) plus a 20% free-space floor; `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` overrides the ceiling outright |
| **In-memory** | FC's memory.bin canonical mmap (`MAP_PRIVATE`) + per-session UFFD-populated divergent pages | host RAM | hardware-enforced COW; one canonical copy serves N sessions of the same image |
| **Metadata** | sessions, conversation log, snapshot manifest refs, hosts, secrets | Postgres | managed/backups |

**Three free COW levels** fall out of content-addressing:

1. **Disk COW** — chunks shared by N sessions referencing the same image. Writes produce new chunks; per-session manifest gets new pointers for dirty offsets. The base manifest never changes.
2. **Memory COW** — the base snapshot's `memory.bin`, captured once per image at enable/rebase time, is `mmap(addr, len, MAP_PRIVATE, fd, 0)`'d by every session of that image. Hardware-enforced via the MMU; one canonical copy serves every session.
3. **Session-fork COW** — `fork_manifest` is a few-KB shallow copy of the parent's chunk list. The data layer is shipped; the `POST /sessions/:id/fork` API endpoint is descriptive only at the chunk-store layer today.

Two consumers turn manifests into running VMs:

- **NBD daemon** (`engram-host-agent::disk_daemon`, Linux + FC only) — serves chunked disks to Firecracker over `/dev/nbdN`. Direct kernel ioctls; no `nbd-client` userspace required. On-demand reads stream from `BlobStorage` via the chunk cache; writes coalesce into per-chunk dirty buffers and flush on snapshot.
- **UFFD handler** (`engram-uffd-handler`, Linux + FC only) — companion process that owns the `userfaultfd(2)` for the VM's memory region. On page fault, resolves the byte offset to a chunk (either canonical `MAP_PRIVATE` hit or session-divergent chunk fetch) and `UFFDIO_COPY`s the page. Working-set traces record the first-N-seconds chunk-access trace per `(manifest, host)`; subsequent restores prefault those chunks before vCPUs run.

The macOS VZ backend uses `materialize-to-file` instead (chunks → assembled `.ext4` before VM start, APFS `clonefile(2)` for per-sandbox COW); memory chunking is FC-only (VZ's native memory snapshot is broken upstream for arm64 Linux guests).

### Wire protocols

Three protocol layers, each with explicit version negotiation where it matters.

| Layer | From → To | Wire | Versioning |
|---|---|---|---|
| **Public API** | clients → coord | JSON over HTTP + SSE | `/v1/` prefix planned |
| **Control plane** | coord ↔ host-agent | bincode `Frame` over WebSocket | `WIRE_VERSION=1` in `Hello`; mismatch refuses connection |
| **In-guest** | host-agent ↔ agentd / bootstrap / harness | length-prefixed bincode over vsock (FC) or virtio-console (VZ) | first-frame token handshake |
| **Image distribution** | materializer → OCI registry | standard registry pull | plain OCI/Docker image (ADR 0080); no engram-specific media types — agentd / harness / guest-tools ride host-staged bundles |
| **Storage** | chunk-store ↔ blob backend | `BlobStorage` trait | impl-specific (GCS, S3, local fs) |

**Control plane wire** (`engram-protocol`). The trait the wire serves is `HostClient` (`engram-core::traits::host_client`), which composes the sandbox surface (FC/VZ/Process) with harness routing (bind/unbind/send_prompt) and a couple of admin operations. `RemoteHostClient` in `engram-protocol::client` is the coord-side wrapper; `LocalHostClient` in `engram-host-agent::host_client` is the host-side composition. ADR 0011.
- `RequestKind`:
  - sandbox lifecycle — `CreateSandbox`, `DestroySandbox`, `ListSandboxes`, `ExecStart`, `Snapshot`, `Restore`, `StartAgent`, `GuestIp`;
  - harness routing — `BindHarnessSession`, `UnbindHarnessSession`, `SendHarnessPrompt`;
  - admin — `ResolveRegistryAuth` (host→coord for OCI creds), `ReapMaterializeDir` (coord→host for materialize-dir GC fanout).
- `ResponseKind`: matched roughly 1:1 with `RequestKind`; harness ops share a single `HarnessOk` ack since their failure modes flow on the `RemoteError` path.
- `StreamItem`: `ExecStdout`, `ExecStderr`, `ExecExit` (terminal). One streaming RPC = one request id; demuxed by `ConnectedHost`.
- `NotifyKind`: `Hello`, `Heartbeat`, `HeartbeatAck`, `SessionEgressPolicy` (coord→host, per-session allow-list + secrets keyring), `HarnessEvent` (host→coord, per-event forwarding so a Claude/noop adapter event in mode=coord+host lands on the coord's SSE bus the same way it would in mode=all).

Nothing's deployed externally yet, so `WIRE_VERSION` is just `1` — every wire-incompatible change bumps it; the mismatch check at hello refuses the connection.

### Session lifecycle

```
POST /sessions
  ↓
Coord scheduler picks a host:
  - snapshot affinity (if resuming)
  - capacity-fit
  ↓
host-agent.PooledBackend.create(spec)
  ├─ ImageCache.ensure_image(uri) → chunked rootfs manifest
  │   (materialized host-side at enable/rebase time, ADR 0080)
  ├─ resolve_rootfs(uri, cached):
  │   - NBD daemon path (Linux + FC + ENGRAM_NBD_DEVICES set)
  │   - materialize-to-file (anywhere else, from chunks)
  └─ inner.create(spec with rootfs_source = resolved_path)
     ├─ FC: spawn firecracker; PUT machine-config / boot-source /
     │      drives/rootfs / vsock; PUT actions InstanceStart
     ├─ VZ: APFS-clone bake rootfs; spawn VZVirtualMachine
     └─ Process: tokio::process::Command in per-sandbox cwd

POST /sessions/:id/exec
  ↓
sandbox.exec_stream(id, ExecRequest)
  ├─ FC: connect to vsock UDS; CONNECT 1024; WireExecRequest;
  │      stream WireExecEvent { Stdout, Stderr, Exit }
  ├─ VZ: same wire shape over virtio-console port 1024
  └─ Process: spawn child; pipe stdout/stderr → ExecEvent stream

[idle TTL exceeded or POST /sessions/:id/snapshot]
  ↓
sandbox.snapshot(id) → SnapshotMetadata { id, disk_manifest, memory_manifest }
  ├─ FC: PATCH /vm Paused; PUT /snapshot/create; PATCH /vm Resumed
  ├─ VZ: pause; APFS-clone the rootfs into the snapshot dir; resume
  └─ Process: tar+gzip the cwd
  ↓
PooledBackend wraps:
  ├─ NBD: backend.flush() → new disk_manifest version
  ├─ Memory: chunk_file(memory.bin, Memory) → memory_manifest
  └─ Coord persists metadata on snapshots row

[next exec/prompt/SSE-subscribe]
  ↓
ensure_active: status Idle → resume
  ↓
sandbox.restore(metadata)
  ├─ PooledBackend.materialize_memory_if_missing (from chunks if
  │   host-local cache lost it; cross-host migration case)
  └─ inner.restore(metadata) → FC PUT /snapshot/load (UFFD or File)
                              → VZ clone rootfs back, fresh VM
                              → Process untar
```

### Workspace

```
crates/
  engram-core                       # types, traits, errors. No I/O.
  engram-chunk-store                # content-addressed chunks + versioned manifests
  engram-protocol                   # wire types: bincode-over-WS Frame protocol
  engram-harness-proto              # wire types: harness ↔ host
  engram-harness-noop               # first-party test harness
  engram-harness-claude             # Claude Code adapter — reference agent harness
  engram-transport                  # vsock (FC) / virtio-console (VZ) abstraction
  engram-coordinator                # binary: HTTP API + scheduler + GC scheduler
  engram-host-agent                 # binary: per-host daemon (chunked-OCI, NBD, reaper)
  engram-rootfs-materializer        # OCI image → whiteout-flattened ext4 + chunks (host-side)
  engram-agentd                     # binary: in-guest exec daemon + harness supervisor
  engram-uffd-handler               # binary: userfaultfd page-fault handler
  engram-sandbox-firecracker        # SandboxBackend: FC microVMs (Linux production)
  engram-sandbox-vz                 # SandboxBackend: Apple VZ (macOS Apple Silicon)
  engram-sandbox-process            # SandboxBackend: subprocesses (DEV ONLY)
  engram-cloud-{gcp,static,mock}    # CloudBackend impls
  engram-secrets-{dev,gcp}          # SecretStore impls (env/dotenv; GCP Secret Manager)
  engram-storage-{local,gcs,s3}     # BlobStorage impls
  engram-oci, engram-oci-auth       # OCI registry client + auth resolver
  engram-postgres                   # MetadataStore impl
  engram-crypto                     # envelope encryption (KEK wrap, registry creds)
  engram-egress-proxy               # per-host egress proxy (ADR 0006)
```

### Sandbox backends

The orchestration layer is VMM-agnostic — anything that implements `SandboxBackend` plugs in. Three ship today:

| Backend | Isolation | Snapshots | Where it runs | When to use |
|---|---|---|---|---|
| `engram-sandbox-process` | **None** — host subprocess | Tarball of workdir | Anywhere (macOS, Linux) | Iterating on the orchestration layer without firing up a VMM. |
| `engram-sandbox-vz` | microVM (Hypervisor.framework) | APFS clone of rootfs | macOS 12+ on Apple Silicon | Mac dev with real microVM isolation. Sub-second cold boot, sub-second cold resume. ADR 0003. |
| `engram-sandbox-firecracker` | microVM (KVM) | FC memory snapshot + UFFD lazy paging + NBD chunked disk | Linux + KVM | Production. Real isolation, real resource enforcement, sub-100ms hot resume. |

`process` and `vz` are dev-side: `process` for the fastest possible iteration loop, `vz` for fidelity to the FC code paths (same `engram-agentd` + harness binaries; same chunked-OCI session-create semantics). Production isolation is always Firecracker.


## Sessions are OCI-image sandboxes with chunked-immutable durability

A session is one bounded unit of agent work. It's two things on the wire: an `image` (a plain OCI image URI — `docker build && docker push`, ADR 0080) and an optional `harness` (which agent process to attach). The image's `/workspace` is the workspace; the platform doesn't run any git operations itself.

```
create → Created → Active                                  (sandbox bound; then agentd up + harness running)
       → idle TTL → Idle                                   (snapshot taken, chunks in BlobStorage, VM destroyed)
       → next prompt/exec → Created → Active               (chunked resume re-runs the create-shape transitions)
       ... loop ...
       → host heartbeat-loss → HostLost → Idle / Dead      (recoverable snapshot → Idle; none → Dead)
       → chunked manifests unreachable / hard TTL → Dead   (terminal)
       → DELETE /sessions/:id → Completed                  (terminal)
```

The state machine is ADR 0015 M2 — `SessionState` in `engram-core::types::session`, with a single validated `transition_session` trait method behind every `UPDATE sessions SET status`. **Active is honest**: the row reaches it only after `start_agent` returns OK, so `/exec` / `/shell` / `/prompt` against an `Active` session no longer race agentd readiness. Earlier states (e.g. `Created`) return typed 409s with state-specific bodies instead of falling through.

`POST /sessions/:id/resume` is a single-tier dispatcher: **Idle** → restore from the snapshot's chunked manifests (snapshot-affinity-scheduled to the host that captured it); **Dead** / **HostLost** without a recoverable snapshot → 410 Gone; anything else → 409. `ensure_active` auto-resumes Idle sessions on the next exec/prompt/SSE-subscribe so callers don't have to know whether the session is live or paused.

**Materialize-dir reap keeps host-local disk bounded.** `POST /api/admin/reap-materialize-dir` reaps the per-host assembled-`.ext4` file cache; in `--mode=coordinator` it fans out across every connected host via WS-RPC. The chunk-store GC that previously paired with this was removed 2026-05-23 (see ADR 0015 M5 "Known regression — chunk-store GC deleted"); BlobStorage cost grows unbounded until a redesigned sweep ships.

**Agents push code from inside the sandbox** when their task is code-edit-shaped. Mount `GITHUB_TOKEN` (or an SSH key) via the image manifest's `[secrets.GITHUB_TOKEN]` block; the secret flows into the harness's env via the same pipeline that delivers `CLAUDE_CODE_OAUTH_TOKEN`. The platform doesn't care whether the agent runs `git push` or `slack.post()` or anything else — that's the agent's job, not the platform's.

**What survives what:**

| Event | Disk chunks | Memory chunks | Conversation log | In-memory state |
|---|---|---|---|---|
| Hot-resume (same host, NVMe cache hit) | preserved | preserved | preserved | preserved |
| Resume on a different host | rebuilt from chunks | rebuilt from chunks + canonical mmap | preserved | preserved |
| Whole-fleet loss (chunks reachable in BlobStorage) | preserved | preserved | preserved | preserved (cold-boot in seconds) |
| BlobStorage backend lost / Dead | gone | gone | preserved (Postgres) | lost |
| Mid-run host crash before snapshot | gone | gone | up to last persisted event | lost |

## Auth

Set `ENGRAM_AUTH_TOKENS` (comma-separated) on the coordinator to require bearer-token auth on every endpoint except `/healthz` (liveness) and `/readyz` (readiness — Postgres ping; for LB probes). Empty = auth disabled (dev default). Clients that call the coordinator directly (the orchestrator, scripts) send `Authorization: Bearer <token>`; the product CLI never does, because it talks to the orchestrator.

```bash
ENGRAM_AUTH_TOKENS=alpha,beta cargo run -p engram-coordinator
```

## Running on real Firecracker

The dev backend is for orchestration iteration. Real Firecracker needs Linux + KVM:

- A Hetzner AX-series box (~€40/mo, no nested-virt tax)
- A GCE n2d / n2 / c3 instance with `--enable-nested-virtualization`
- A Linux laptop or workstation
- CI

On a Linux + KVM host, `just dev` auto-detects `/dev/kvm` and runs the Firecracker backend (no flag, no per-arch recipe — ADR 0024); `just pull-kernel` fetches a vmlinux into the standard cache. The rootfs is materialized host-side from the enabled OCI image into chunked ext4 (ADR 0080); `engram-agentd` is not baked in — it rides the fleet `bundle-agentd` slot and the stage-1 init execs it, so `exec_stream` works against any plain image.

The crate ships an integration suite that covers the full surface against real microVMs:

```bash
# On a Linux + KVM host, after building engram-uffd-handler + engram-agentd:
bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh all
```

Tests: `boot` (kernel banner on serial), `lifecycle` (create/list/destroy round-trip), `snapshot` (file-backed restore), `snapshot_uffd` (lazy paging via the userfaultfd handler), `exec_real_vm` (busybox-fixture rootfs → boot → exec through the in-guest agent over vsock), `nbd_chunked_disk` (chunked disk via NBD daemon), `canonical_capture` (base-snapshot memory capture).

For UFFD-backed restore, the coordinator's host needs:
- `/dev/userfaultfd` mode 0666 (set via udev rule, see `dev-vm/scripts/bootstrap-remote.sh`)
- `vm.unprivileged_userfaultfd = 1` sysctl
- `engram-uffd-handler` binary on `$PATH` (or set `FirecrackerConfig::uffd_handler_bin`)

For NBD chunked disks, the host needs:
- `CONFIG_BLK_DEV_NBD=y` in the kernel (Ubuntu cloud images — yes)
- `modprobe nbd nbds_max=<N>` at boot
- `ENGRAM_NBD_DEVICES=/dev/nbd0,/dev/nbd1,...` set in the host-agent env

For the in-guest agent, the fleet needs:
- A static-musl `engram-agentd` published as the `bundle-agentd` slot (ADR 0080) and staged on each host — the stage-1 init copies it out of the bundle to tmpfs and execs it (no rootfs bake).
- `init=/sbin/engram-init` in the kernel boot args (set by the FC backend via `FirecrackerConfig::default_boot_args`)
