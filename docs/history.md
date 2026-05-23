# History — phase chronology

Engram shipped in roughly seven phases between Q1 and Q2 2026. This
document is the milestone log — what landed in each phase, what
later ADRs revised, and where the implementation lives today.

Architectural rationale lives in `docs/adr/`. This page is the
"when" axis; ADRs are the "why."

For the live punch list (what's still in flight + what's deferred),
see [`docs/chunked-storage-rollout.md`](./chunked-storage-rollout.md).
For the current system architecture, see the [README](../README.md).
For deployment, see [`docs/deploy.md`](./deploy.md).

---

## Phase 1 — Orchestration layer end-to-end on the dev backend

**Goal**: one binary, one host, one repo, end-to-end session,
runnable on macOS Apple Silicon.

- `engram-coordinator` HTTP API: `POST /sessions`, `GET/DELETE /sessions/:id`,
  `POST /sessions/:id/exec` + `/exec/stream` (SSE), snapshot/resume,
  `GET /sessions/:id/events` (SSE replay via `?since=N` /
  `Last-Event-ID`).
- Bearer-token auth middleware (constant-time compare, `/healthz`
  exempt).
- `engram-sandbox-process` real implementation: per-sandbox cwd,
  real subprocess exec, tarball-based snapshot/restore.
- Pluggable `SecretStore` (env / dotenv / GCP-Secret-Manager-stub).
- `engram-cloud-{static,gcp,mock}`, `engram-postgres`.
- Postgres schema + migration runner.
- Persistent session-event log + SSE bus, `Last-Event-ID` reconnect.

**Deliverable**: `just dev` brings up Postgres + coordinator
(subprocess backend) on a Mac. `curl POST /sessions` returns a
session, `POST /sessions/:id/exec` round-trips real stdout/stderr/
exit-status.

---

## Phase 2 — Firecracker integration (Linux production path)

**Goal**: production-grade isolation, snapshot-evict mechanic
working end-to-end.

- `engram-sandbox-firecracker` real implementation. Manual HTTP/1.1
  over `tokio::net::UnixStream` (no `hyper`; the protocol is small
  and explicit). VM config via Firecracker's
  `PUT /machine-config` + `/boot-source` + `/drives/rootfs` +
  `/vsock`, then `PUT /actions InstanceStart`.
- `engram-agentd` (in-guest exec daemon, vsock listener) + the
  init shim. Built static-musl; injected into rootfs by the image
  baker.
- Snapshot/restore via Firecracker API:
  - Take: `PATCH /vm Paused` → `PUT /snapshot/create` → `PATCH /vm Resumed`
  - Restore: `PUT /snapshot/load` with `backend_type=File`
    (synchronous) or `backend_type=Uffd` (lazy paging)
- **UFFD memory backend** (`engram-uffd-handler`) — separate
  companion process. Receives the userfaultfd via SCM_RIGHTS, mmaps
  `memory.bin`, services page faults with `UFFDIO_COPY`. Sub-100ms
  resume regardless of guest RAM.
- Image baker `Format::Ext4` mode — `mke2fs -t ext4 -F -d <staging> <out>.ext4`.
  No loopback mount, no root.
- Five integration tests against real microVMs on the GCP dev VM —
  `boot`, `lifecycle`, `snapshot`, `snapshot_uffd`, `exec_real_vm`.

---

## Phase 3 — Multi-host coordinator

**Goal**: two hosts working together; new hosts come online with
zero coordinator-side config.

- Split binaries: `engram-host-agent` runs separately, dials
  coordinator over WebSocket. `--mode=all` registers the local
  backend in-process so `just dev` stays single-binary.
- Wire transport: bincode-encoded `Frame` enum
  (`Request`/`Response`/`Stream`/`Notify`) over WebSocket binary
  messages. Hand-rolled `request_id` demuxer routes Responses to
  `oneshot`s and Stream items to per-id `mpsc`s. **Decision flip
  from the original design**: bincode-over-WS replaced the
  originally-planned tonic+gRPC — reuses the existing `engram-agentd`
  framing pattern and avoids a separate `.proto` file + `build.rs`
  for an internal-only channel.
- `/api/hosts/connect` axum handler under the existing bearer-auth
  middleware.
- Host-agent dialer with exp-backoff reconnect, `NotifyKind::Hello`
  first frame, periodic heartbeat.
- `HostRegistry` — coordinator-side, implements `SandboxBackend`.
  Tracks `sandbox_id → host_id` ownership for routing.
- Real scheduler — `pick_for_session(ctx)` ranks: snapshot affinity
  → warm-pool match → capacity-fit → any non-draining fallback.
- Coordinator HA via Postgres `LISTEN/NOTIFY` on `session_events` +
  `host_dead`. Replicas re-broadcast events into local SSE
  subscribers and drop dead hosts in lockstep.
- Dead-host auto-detector — polls `list_stale_hosts` every ~10s,
  races other replicas via `pg_try_advisory_lock`, atomically marks
  the host `Dead` + reassigns sessions.
- `sessions.sandbox_id` persistence + `repopulate_routing` on coord
  startup so Active sessions survive a coordinator restart.
- `GET /api/hosts` + `engram host list/get/drain` CLI shipped.

---

## Phase 4 — Hot suspend + harness protocol

**Goal (post-amendment)**: hot-suspend mechanics + the harness
protocol.

Originally framed as "git as the workspace durability primitive"
(ADR 0001) — agents would `git push` checkpoints and Engram would
manage the branches. Phase 6 (ADR 0005) retired that framing: git
is no longer a platform-layer concern. Agents that want to push
code do it themselves inside the sandbox using credentials mounted
via `[secrets.GITHUB_TOKEN]`.

What stayed from Phase 4's original scope:

- Hot-suspend mechanics: idle TTL → FC memory snapshot → destroy
  the VM → mark `Idle`. Next prompt/exec auto-resumes from the
  snapshot.
- Harness protocol (`engram-harness-{proto,noop,claude}`) — wire
  types + adapters; in-VM `engram-bootstrap` supervisor; host-side
  `HarnessHub` TCP forwarding harness events into `session_events`.
- Preemption best-effort drain (Track D) — host-agent subscribes to
  `cloud.preemption_signal()`; on notice fans out drain RPCs in
  parallel with a 25s deadline.
- Claude Code adapter (`engram-harness-claude`) shipped as the
  reference adapter; runs end-to-end against real Firecracker
  microVMs.

ADRs 0001 + 0002 captured the original framing; ADR 0005 amended
it.

---

## Phase 4.5 — Apple Silicon backend (April 2026)

**Goal**: real microVM isolation for Mac contributors, exercising
the same in-VM code paths as Firecracker locally instead of
round-tripping every test through a remote Linux dev VM.

- `engram-sandbox-vz` drives Apple's Virtualization.framework
  directly from Rust via `objc2-virtualization` — no Swift driver.
  ~1500 LOC across `vm` (lifecycle), `console_bridge` (multi-port
  virtio-console host↔UDS pump), `disk` (APFS clone helper),
  `snapshot` (clone-based manifest), `backend` (the trait impl).
- **Multi-port virtio-console** for the host↔guest control plane —
  universally supported by Linux kernels (no `CONFIG_VIRTIO_VSOCKETS=y`
  requirement). Three logical ports: 1024 = agentd, 1025 =
  bootstrap, 1026 = harness adapter.
- `engram-transport` — backend-agnostic trait abstraction over
  vsock (FC) and virtio-console (VZ). All four in-VM binaries
  dispatch on `ENGRAM_TRANSPORT=vsock|console` at runtime.
- **APFS-clone-based snapshots** replace VZ's native
  `saveMachineStateToURL`/`restoreMachineStateFromURL` — that pair
  is broken upstream for arm64 Linux guests on macOS (UTM #6654,
  Apple DevForum 745168, Apple's own `containerization` framework
  avoids the API for the same reason). The clone *is* the
  snapshot; restore clones it back into a fresh per-sandbox rootfs
  and cold-boots a new VM. `engram-bootstrap` supervisor +
  `claude --resume <id>` carry conversation continuity across the
  cold boot. APFS `clonefile(2)` runs in ~50 ms even on a 1.7 GB
  ext4 rootfs.
- **Sub-second timings**: cold boot 562 ms, cold resume 746 ms,
  warm-pool checkout 30 ms (same `PooledBackend` wrapper as FC).

ADR 0003 captures the design.

---

## Phase 5 — Registry-backed image + harness distribution

**Goal**: bake images + harness packs become OCI artifacts in a
standard Docker registry; coordinator becomes stateless w.r.t.
image data.

- `engram-image-builder` pushes images as custom OCI artifacts
  (`application/vnd.engram.manifest.v1+toml` for the manifest,
  `application/vnd.engram.rootfs.ext4` for the disk image, plus
  ADR 0007's `application/vnd.engram.bundle.v1+json` for the
  chunked-manifest pointer).
- Host-agents pull on first use into a content-addressable cache
  (`<work_dir>/oci-cache/sha256/<digest>/`).
- Registry credentials live in Postgres envelope-encrypted under a
  deployment KEK (`engram-crypto`); `RegistryAuthSpec` is
  variant-discriminated so static creds, GCP Workload Identity,
  and future cloud IAM kinds all dispatch through one resolver.
- `engram registry` / `engram harness` CLI surface.
- Per-deployment local dev via `registry:2` in `docker-compose`.

ADR 0004 captures the design.

---

## Phase 6 — Hot+cold snapshot durability (May 2026)

> **Superseded by ADR 0007.** The two-tier hot/cold framing
> shipped, then retired in Phase 7 in favour of chunked-immutable
> storage as the single durability primitive. This phase is
> historical record only.

ADR 0005 reintroduced the cold blob tier after ADR 0001 had
retired it. The premise shifted from "30-second-preemption
replication" (infeasible math) to "minutes-budget disk-pressure
flush" (trivial math).

- **Hot tier** (per-host local NVMe): FC `memory.bin` + state file
  on Linux, APFS rootfs clone on VZ. Sub-second resume on the
  same host.
- **Cold tier** (`BlobStorage` — S3/GCS/local fs): tar+zstd of the
  FC snapshot dir, sealed under the deployment KEK in Postgres.
- Three-branch resume dispatcher: Idle (hot) → ColdEvicted (cold)
  → Dead (410 Gone).
- Disk-pressure detector + admin `POST /api/admin/sessions/:id/flush`
  endpoint — implicit + explicit triggers share the same
  `flush_session` primitive.
- ADRs 0001 + 0002 amended/superseded; git fully retired from the
  platform layer.

ADR 0005 captures the design; ADR 0007 supersedes the two-tier
framing.

---

## Phase 7 — Chunked-immutable storage + production deploy (May 2026)

**Goal (two intertwined)**:

(a) The durability primitive moves from tar+zstd cold-tier to
**chunked-immutable content-addressed storage** — 16 MiB disk
chunks + 512 KiB memory chunks in `BlobStorage`, manifests
versioned per session.

(b) **Cloud-agnostic deployment artifacts** — a Helm chart for the
coord, Packer manifest + GCP Terraform reference module for FC
hosts.

What shipped:

- **Chunk store** (`engram-chunk-store`) — content-addressed by
  sha256, versioned manifests, LRU local cache with pinning +
  singleflight, GC by manifest reachability + retention window.
- **Image-builder** chunks the ext4 at bake time + writes a
  `bundle.json` sidecar pointing at the disk manifest. Bake-time
  canonical memory capture: boot the rootfs once, pause, chunk
  `memory.bin`, write the canonical manifest into the bundle.
- **VZ snapshot path** chunks the cloned rootfs into the store.
- **NBD daemon** (`engram-host-agent::disk_daemon`) serves chunked
  disks to FC over `/dev/nbdN`. Direct kernel ioctls — no
  `nbd-client` userspace required. Linux + KVM only.
- **UFFD-from-chunks** — `engram-uffd-handler` resolves FC page
  faults against a chunked memory manifest + canonical-base
  `MAP_PRIVATE` mmap. Working-set traces record + replay across
  hosts for fast restore.
- **Migrations 0018–0020**: `disk_manifest_id` + `memory_manifest_id`
  columns added; cold-tier columns + `SessionStatus::ColdEvicted`
  + `SnapshotResidency` enum + `wrapped_dek/nonce/ciphertext/key_id`
  envelope-encryption quartet on snapshots all dropped.
- **`engram_coordinator::chunk_gc::spawn`** — background cron that
  fired `chunk_gc::run_once` on the `ENGRAM_CHUNK_GC_INTERVAL_SECS`
  cadence (default 1h). `POST /api/admin/gc-chunks` delegated to
  the same pipeline so explicit-trigger and cron-driver shared
  code. **Removed 2026-05-23** after a prod incident; see ADR
  0015 M5 "Known regression — chunk-store GC deleted."
- **`POST /api/admin/reap-materialize-dir`** — in-proc in `--mode=all`,
  multi-host WS-RPC fanout in `--mode=coordinator` via
  `HostAdminHandler` (WIRE v3).
- **Phase 6 destructive trait reshape** (`SandboxBackend::snapshot(id)`
  + `restore(metadata)` + `snapshot_path_for(id)`). Backend owns its
  per-snapshot staging dir; coord no longer dictates layout.
  WIRE v4.
- **Deployment artifacts**:
  - `deploy/helm/engram-coordinator/` — Helm chart, cloud-agnostic
    templates with `values-gcp.yaml.example` / `values-aws.yaml.example`.
  - `deploy/packer/fc-host-gcp.pkr.hcl` — GCE image build with
    Firecracker + static-musl `engram-host-agent` + systemd units +
    drain hook.
  - `deploy/terraform/gcp/modules/{network,storage,fc-host-mig}` —
    plus `examples/minimal/` wiring them. Operator-provided modules
    expected for GKE / Postgres / Secret Manager / Artifact Registry
    (rationale in `deploy/terraform/gcp/README.md`).
  - Operational reference: [`docs/deploy.md`](./deploy.md).

ADR 0007 captures the design + the explicit deferred-decision list
(observability, AWS Terraform, L2 regional shared cache). Live
punch list: [`docs/chunked-storage-rollout.md`](./chunked-storage-rollout.md).

---

## Phase 8 — Chunks-in-OCI + warm-pool retirement (May 2026)

**Goal**: make the OCI image URI self-contained again (ADR 0007's
BlobStorage-as-source-of-truth bricked cross-namespace pulls), and
take the resulting cold-start improvement to retire warm pools.

- **Chunks-in-OCI with tiered cache (ADR 0008)** — bake produces
  Nydus-shaped artifacts (`bootstrap_disk.json` + `chunks_disk.blob`
  layers; same shape for memory chunks when canonical-memory
  capture ran). Chunk fault path becomes
  `NVMe → BlobStorage → OCI registry`, with CDN-fill tee-back to
  BlobStorage on registry hits. `ChunkResolver` trait in
  `engram-chunk-store`; `TieredChunkResolver` composes the two
  backends with the NVMe cache.
- **Closure-based `ChunkCache`** — refactored so the cache holds no
  reference to a specific `ChunkStore`; callers pass a fetcher
  closure per-`get`. Lets the host swap in a tiered resolver for a
  specific session without rebuilding the cache, and avoids the
  cross-namespace bricked-image bug we shipped in Phase 7.
- **Atomic materialize** — `materialize_to_file{,_cached}` write to
  `<dest>.partial-<nonce>` then atomic rename. Fixes a kernel-panic
  failure mode where a partial write left a zero-byte file under
  the canonical name; subsequent fast-path checks served the
  zero file forever.
- **Warm pool deletion (WIRE v5)** — the pre-Phase-8 host-side
  warm pool (`Pool` + `PoolKey` + replenish loop) shipped through
  Phase 3 with a series of correctness sharp edges (key ignored
  `repo`, then ignored `harness_substrate`, then needed
  `rootfs_source` to be a content-addressed digest). With
  chunked-OCI restore + canonical-memory bringing cold start
  under the warm-pool checkout time, the lifecycle complexity
  stopped paying for itself. Deleted entirely: heartbeat shape no
  longer carries `warm_pools`, scheduler no longer ranks on
  warm-slot affinity, host-agent no longer replenishes. `Heartbeat`
  wire shape changed → `WIRE_VERSION = 5`.

ADR 0008 captures the design (chunks-in-OCI, scanner mitigation,
base/diff layers, alternative paths considered). Warm-pool deletion
rationale lives in the heartbeat doc-comment + this entry.

---

## Phase 9 — Portable snapshots + warm pool, take two (May 2026)

**Goal**: sub-second session create on cold templates, and a
durability story for in-flight sessions across host loss / MIG roll.
The Phase-8 retirement of warm pool removed a *fanout* mechanism;
chunked-OCI cold start is fast, but not 100 ms fast, and it can't
help durability at all. Portable snapshots are the missing primitive
for both.

- **Portable-snapshot artifact (ADR 0014 M1.2)** — FC's `state.bin`
  + sidecar JSON join the existing chunked memory + (future) chunked
  disk in BlobStorage. State.bin embeds host paths verbatim, so the
  receiver must materialize the same paths it embeds — enforced by
  the canonical jail-layout contract (M1.1).
- **Canonical path scheme (ADR 0014 M1.1, M1.2a)** — rootfs symlink
  + harness symlink + vsock UDS all live outside the jail dir at
  source-sandbox-id-keyed paths under `<work_dir>`. Survive
  `destroy()`'s `remove_dir_all(jail_dir)`, so cross-host restore
  (warm-pool or durable resume) finds them intact.
- **`templates` table + resolver (ADR 0014 M1.4)** — maps
  `(image_repo, image_tag, harness_pack_uri) → snapshot_id + vcpus
  + memory_mib`. Rebake flips prior row to `active=false`; coord
  ships the live active set in every heartbeat-response.
- **Per-host `WarmPool` driver (ADR 0014 M1.6)** — free-list per
  template_ref, refill loop bounded by an atomic-Entry inflight
  guard (caught by dev-VM e2e, hotfix `34b18aa`). v1 ceiling at
  N=1 per template per host because concurrent restores collide on
  the source-keyed vsock UDS; mount-namespacing per FC unblocks
  N>1.
- **Warm-lease scheduler path (ADR 0014 M1.8)** — `pick_for_session`
  resolves template_ref → `candidates_with_warm_slot` (heartbeat
  warm_slots) → sequential `LeaseWarmSandbox` → `LaunchWarmSandbox`.
  Lease failure (stale ref / no capacity / launch error) falls
  through to cold-create with one tracing::warn.
- **Lease-rate autoscaler (ADR 0014 M1.9)** — per-template lease
  history over 5-min window; `target = clamp(ceil(rate × refill ×
  1.2), FLOOR=1, CEILING=1)`. CEILING rises with N>1 work.

E2e-validated on the dev VM 2026-05-16 (commits `34b18aa` +
`e96842d` + `a803fda` were dev-VM-found, not unit-test-found —
documented at length in the ADR's "Verification" section). M2
(durability — `Paused` status, drain-time snapshot upload, Resume
API) is queued but not yet started.

ADR 0014 captures the full design + the explicit two-milestone
shape; ADR 0012 ("warm pool deferred") flips from `deferred` →
`landed via 0014`.

---

## Phase 10 — System design v2: in-VM service unification + host-image readiness (May 2026)

**Goal**: address two recurring bug classes that no amount of
incremental patching had retired. (1) Two in-VM processes
(`engram-bootstrap` on vsock 1025, `engram-agentd` on 1024) with two
readiness signals produced an "Active before agentd is reachable"
race that the no-harness session path kept hitting. (2) The
`templates` table introduced by Phase 9 became the source of
cross-CPU-vendor snapshot tripfaults (forced
`ENGRAM_WARM_POOL_DISABLED=1` in prod) and dangling rows whose
blobs were gone (host-agent's refill loop spammed
`blob_not_found`). ADR 0015 is the system-walk-through that turned
both into structural fixes rather than scab patches.

- **M1 — `GuestService` (April→May 2026)** collapsed
  `engram-bootstrap` into `engram-agentd`. One vsock port (1024),
  one wire protocol (`WireRequest::SpawnHarness`), one readiness
  signal: agentd dials the host on startup, the host's
  `start_agent` blocks on a `tokio::sync::watch` filled by the
  per-sandbox accept task. `connect_fc_vsock_with_retry` deleted;
  the boot-race the no-harness path used to produce is
  structurally impossible because every session goes through
  `start_agent`, which returns only after the ready dial. Commits
  `75ba2df` / `97de278` / `0d444e9` / `4b3f890`.

- **M5 — Host-image readiness (May 22 2026)** retired the
  `templates` table, the warm pool driver, and the bake-time
  canonical capture path in one atomic cutover. Migration
  `0030_drop_templates.sql` drops the table; the OCI artifact
  becomes pure content (chunked rootfs + manifest, no state.bin /
  memory.bin / sidecar.json). Hosts diff coord's
  `enabled_images` against a per-host NVMe `ChunkCache` on every
  heartbeat, prefetch missing chunks via the existing
  `TieredChunkResolver`, and report `ready_images:
  Vec<ManifestDigest>` on the next outbound. The scheduler refuses
  to place sessions on hosts that haven't prefetched the
  requested image — `SandboxError::ImageNotReady` returns HTTP
  503 with an operator-facing hint distinct from "no capacity".
  ~4900 LOC net retired, ~1100 added. Four-commit chain
  `d20e5da` / `18be1ad` / `6fb36e2` / `7ab8625`. End-to-end
  verified on the dev VM. Cold-boot expectations: prod target
  ~3.5 s with chunks-local NVMe reads (down from ~17 s of
  on-demand GCS page-ins).

  Three field bugs were caught during dev-VM exercise rather
  than unit tests: coord originally materialized chunks at a
  fresh `ManifestRef`, the supervisor went through BlobStorage
  rather than teeing into `ChunkCache`, and `ImageCache::ensure_image`
  returned stale digests on registry re-pushes. All fixed in
  `7ab8625` along with the integration-test script rewrite.

Phases M2–M4 + M6–M8 of ADR 0015 stay Proposed. Warm pool's third
incarnation will be a separate later milestone — host-local
runtime snapshot generation, layered on M5's storage floor;
CPU-vendor dissolved by construction because each host snapshots
on its own CPU.

---

## Cross-cutting

These don't fit one phase but track across the timeline:

- **Test infrastructure** — `cargo nextest`-driven unit + integration
  suite. Live-Postgres integration tests gated on
  `ENGRAM_TEST_DATABASE_URL`. Linux + KVM-gated tests via the
  `dev-vm` skill (`crates/engram-sandbox-firecracker/tests/*.rs`
  with `#[ignore]`, runner script
  `scripts/run-boot-test.sh`).
- **Documentation discipline** — DESIGN.md captures architecture;
  ADRs in `docs/adr/` capture non-obvious decisions; this file is
  the milestone log; `docs/chunked-storage-rollout.md` is the live
  punch list; `docs/known-issues.md` is the operational caveat
  register.
- **Developer ergonomics** — `just dev` stays working through every
  phase. If a phase regresses Mac dev, that's a fix.

---

## What's still open

Tracked live in [`docs/chunked-storage-rollout.md`](./chunked-storage-rollout.md):

- **Observability** — no metrics on the chunked path yet (cache hit
  rate, chunk fetch latency, materialize time, GC counters). Needs
  a framework call between Prometheus exporter and OTel.
- **AWS Terraform module** — GCP shipped; AWS deferred until first
  operator demand.
- **Tier 3 + Tier 4 validation** — real `helm install` against a
  live GKE cluster and `terraform apply` against a live GCP project.
  Modules + chart validate; runtime validation pending.
