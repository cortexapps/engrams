# ADR 0014: Portable snapshots — warm-pool session create + durable resume

Status: accepted, 2026-05-15
Phase: 1 (M1 in flight) — ADR + `paths.rs` contract module landed
(`ccde296`, `5188751`); portable-snapshot primitive (canonical path
wiring + mount-namespace per FC + state.bin/sidecar upload + cross-
host restore integration test) is the next coherent commit.

## Context

Two problems with the same root cause.

**Session create is slow.** Every session today is a cold microVM boot.
First-touch per template per host pays OCI pull → chunk + upload → cold
kernel boot with serial NBD-chunked rootfs reads → engram-init →
bootstrap → harness. We measured 30–75 s on the cold path in production
during ADR 0013 rollout. Steady-state (chunks already in GCS, kernel
binary local) is still ~5–15 s, bounded by the kernel boot + vdb ext4
mount, which is fundamentally serial. The 100 ms / sub-second session-
create promise the chunked-memory substrate was built for has never
been delivered, because we built the substrate (chunked manifests, UFFD
prefault, canonical-mmap sharing) but never the driver that consumes it.

**MIG rolls lose session state.** GCE sends SIGTERM; `engram-host-
agent/src/shutdown.rs` snapshots each sandbox to local NVMe and writes
`last_local_snapshot` to `sandbox.json`. The host then dies and takes
the local NVMe with it. `dead_host.rs` flips affected sessions to
`Failed`. The user loses workspace + harness state. Conversation
history is durable in `session_events`; workspace and agent process
state are not. Rolls happen on every image bake, every template
change, every autoheal cycle.

Both problems share one missing primitive: **a snapshot artifact that's
portable across hosts.** Warm-pool wants to fan out one bake-time
template snapshot to N hosts. Durability wants to migrate one session
snapshot from a dying host to a new one. Same operation, different
trigger.

The infrastructure carries us most of the way:

- FC `snapshot()` / `restore()` are wired end-to-end with UFFD chunked-
  memory; restore wall-clock is ~100–500 ms (`crates/engram-sandbox-
  firecracker/src/lib.rs:1250-1379`).
- Bake-time canonical capture exists (`engram-image-builder`
  `CanonicalCaptureConfig`) — it boots a rootfs under FC, settles,
  snapshots, chunks memory, stores ref in `bundle.json`.
- `engram-bootstrap` is a **long-lived supervisor** on vsock 1025
  (`crates/engram-bootstrap/src/main.rs:178`). It loops on `accept()`
  and `cmd.spawn()`s the agent. Bootstrap survives every snapshot/
  restore boundary. After restore, the host dials, pushes a fresh
  `BootstrapLaunch{argv, env}`, bootstrap reaps the prior agent child
  and spawns the new one. Same rehydration path for warm-launch and
  Resume.
- Memory chunks are already in `BlobStorage` (chunked manifests, ADR
  0007). FC `state.bin` and the sidecar JSON are local-only and need
  to become portable.
- Canonical memory uses `mmap(MAP_PRIVATE)` (not KSM) — N concurrent
  VMs of the same template share the canonical mmap via the kernel
  page cache. This is the cost mechanism that makes warm-pool depth
  affordable.

## Decision

Single ADR, two milestones, substrate-only snapshot semantics.

**Substrate-only snapshots.** A snapshot captures kernel + bootstrap +
page caches + filesystem. The agent process is **not** preserved
across thaw. Resume cold-restarts the agent against on-disk transcript
via a fresh `BootstrapLaunch`. This avoids the TCP-streams-break-on-
thaw failure mode (Claude API keepalive state, coord WS to harness, all
broken on thaw) and unifies the rehydration path between warm-pool
launches and durable Resume. Honest UX contract: "Last active <Xh>
ago — Resume" restarts the agent against persisted state, not a frozen
process.

**M1 ships first: warm-pool.** Validates the portable-snapshot
primitive on the simpler payload (no live agent, no per-session
writable-rootfs delta). Hosts maintain a free-list of pre-restored
microVMs per template, refilled async. Session create leases from the
pool when available; cold-create is the fallback.

**M2 lands after M1 has soaked: durability.** Layers durability
semantics on the validated primitive. Active sessions snapshot on
idle-entry and on graceful drain; artifacts upload to BlobStorage. New
`SessionStatus::Paused` + dead-host reaper transitions affected
sessions; `POST /api/sessions/:id/resume` cold-restarts the agent.

The two milestones land sequentially because warm-pool's payload is
strictly simpler — no live-agent state, no writable-rootfs delta — and
exercises the cross-host restore path on the cheapest possible
artifact. Durability piggy-backs on the validated primitive plus a
writable-rootfs upload extension and the Paused/Resume orchestration.

## Architecture

### The portable snapshot primitive

A `PortableSnapshotRef` is the durable handle any coord pod can hand
to any host to restore:

```
PortableSnapshotRef {
  snapshot_id: SnapshotId,
  memory_manifest:           ManifestRef,        // chunked, in BlobStorage
  canonical_memory_manifest: Option<ManifestRef>,// template canonical
  rootfs_manifest:           Option<ManifestRef>,// chunked disk (when Phase 4 NBD writable lands)
  rootfs_blob_key:           Option<String>,     // M2 interim: tar+zstd writable rootfs
  state_blob_key:            String,             // FC state.bin (small, ~tens KiB)
  sidecar_blob_key:          String,             // FcSnapshotManifest JSON
  template_ref:              Option<TemplateRef>,// which template this is restorable as
}
```

Two new BlobStorage object kinds: `snapshots/<id>/state.bin` and
`snapshots/<id>/sidecar.json`. Opaque blobs, not chunked — both small.

Cross-host restore on the receiver:

1. Download state.bin + sidecar.json from BlobStorage.
2. Memory: existing `materialize_memory_if_missing`
   (`crates/engram-host-agent/src/pooled_backend.rs:973-990`) hydrates
   memory.bin from chunked manifest.
3. Rootfs: chunked manifest if present, else tar+zstd unpack to
   canonical jail path.
4. Path canonicalization (below).
5. Hand to existing `FirecrackerBackend::restore_in_jail`.

### Path canonicalization (correctness prerequisite for M1)

`state.bin` embeds `vsock_uds_path`, TAP name, rootfs `path_on_host`.
FC has no API to rewrite state.bin after capture. The existing restore
code already re-provisions TAP at the manifest-derived name
(`reserve_restored_net`, `lib.rs:1280`) and re-creates the vsock UDS at
the manifest-derived path. The canonical scheme tightens the contract:

- All hosts use identical `<work_dir>` (packer-installed:
  `/var/lib/engram/sandboxes`).
- Jail dir: `<work_dir>/<sandbox_id>/` — per-FC-process runtime
  state (FC api socket, log, uffd uds). Removed on destroy.
- vsock UDS: `<work_dir>/<sandbox_id>.vsock` — outside the jail, so
  destroy's `remove_dir_all(jail_dir)` doesn't break a subsequent
  restore.
- **Rootfs canonical path: `<work_dir>/rootfs/<sandbox_id>.dev`** —
  outside the jail, source-sandbox-id-keyed. Symlink to the actual
  rootfs source (NBD device, materialized file, etc.). FC put_drive
  receives this path; state.bin embeds this path. The path
  survives destroy of the source sandbox so cross-host restore (or
  same-host idle resume) can recreate the materialization.
- **Harness canonical path: `<work_dir>/harness/<sandbox_id>.ext4`**
  — same shape.
- Snapshot capture refuses non-conformant paths (fail-fast at the
  source rather than fail-mysteriously at restore time on a
  different host).

### Per-FC mount namespace for concurrent restores

A snapshot may be restored multiple times concurrently (N warm
slots from one template; a leased slot whose refill is in-flight).
Every restored FC opens the same embedded `path_on_host` from
state.bin. Sharing one file across N writable FCs corrupts the
rootfs.

The fix is `unshare(CLONE_NEWNS)` per FC process: each FC runs in
its own mount namespace where the canonical path bind-mounts to a
per-FC writable copy. The host's view keeps the canonical path
unchanged; the FC's view sees its own private file at the same
path.

Implementation: `Command::pre_exec` in the FC child runs the
namespace + bind-mount setup between fork and exec, before FC's
`main()` ever runs. The host process is unaffected.

This is the standard pattern in Lambda's microVM stack (NSDI '20),
FC's own jailer, runc, and kubelet — adopted here because the
"one canonical path, many private views" pattern is exactly what
multi-restore needs.

### Bootstrap-after-restore: the unified rehydration path

Both milestones use the same wire choreography after FC `restore`:

1. Host re-creates TAP + vsock UDS at canonical paths.
2. Bootstrap (alive in the restored guest) is on `accept()`.
3. Host dials bootstrap on the UDS, reads `BOOTSTRAP_READY_BYTE` (0xEB).
4. Host pushes `BootstrapLaunch{argv, env}`:
   - M1 warm-launch: argv = harness, env = session token + initial
     prompt + workspace hints.
   - M2 resume: same shape; the agent itself reads on-disk transcript
     and continues from there.
5. Bootstrap reaps any prior agent child (none on warm-launch; the
   cold-restarted agent on M2 resume), spawns the new one.

Wire surface unchanged from `engram-harness-proto`. No new in-guest
RPCs.

### Cost claim: canonical-mmap page-cache sharing

The UFFD handler mmaps the canonical memory.bin with `MAP_PRIVATE`
(`crates/engram-uffd-handler/src/chunked.rs:14-22`). N concurrent
sandboxes of the same template each get their own mmap of the same
file. The kernel page cache de-duplicates read pages across the
mappings — N warm slots × 2 GiB of memory does **not** cost N × 2 GiB
resident.

This is load-bearing for warm-pool depth economics. It is also
**untested at scale** today — no test exercises multi-restore-from-one-
canonical. M1 ships a bench (`warm_pool_memory`) that boots N=20
sandboxes and validates `MemAvailable` vs the sum of per-cgroup
`memory.current`. If they diverge, cgroup config tweaks
(`memory.swap.max=0`) or a conservative cap on warm-pool depth gate
production rollout. KSM is the documented fallback if shared
accounting can't be made to work; currently rejected for side-channel
reasons.

## Milestone 1 — Warm pool

Goals:

- p50 warm-lease session create ≤ 150 ms; p99 ≤ 250 ms.
- v1 warm-pool depth: N=1 per template per host (mount-namespace
  isolation makes N>1 correctness-safe; the depth-cap is a memory-
  cost decision, see warm_pool_memory bench result).
- Templates with `session_kind ∈ {ephemeral, readonly}` warm-leased by
  default.
- Templates with `session_kind=git` fall through to cold-create.
  Workspace late-bind via virtio-fs is a future ADR.
- Lease failure (stale template, host gone, capacity exhausted) falls
  through to cold-create with one tracing::warn.
- Eager hydration on image upload: a new bake → new `templates` row →
  hosts refill warm slots for it via the coord's active template set,
  without waiting for the first user session. Memory chunks are
  already in BlobStorage from the bake pipeline, so first-touch is a
  GCS pull (intra-VPC, fast) not an OCI registry pull. Eliminates the
  "first user pays the OCI pull cost on a freshly-uploaded image" UX
  gap.

Components:

- **Image builder extension**: `CanonicalCaptureConfig` already
  snapshots memory at bake. Add state.bin + sidecar upload after the
  capture; register a `templates` row pointing at the resulting
  `PortableSnapshotRef`. Snapshot point: bootstrap on `accept()`, after
  writing READY, before any BootstrapLaunch — capture script connects,
  reads READY, immediately issues FC pause + `PUT /snapshot/create`.
- **`templates` table**: maps (image_repo, image_tag, harness_pack_uri)
  → snapshot_id + memory/cpu spec. Rebake flips prior row `active=false`.
- **Per-host `WarmPool`** (new `crates/engram-host-agent/src/warm_pool.rs`):
  free-list per template_ref; refill loop; 60 s grace on rebake.
  v1 effective N(T)=1 default per host. The autoscaler computes
  a target from observed lease rate but **CEILING_TARGET=1** in
  v1: N>1 concurrent restores from one snapshot collide on the
  source-sandbox-id-keyed vsock UDS path (FC's state.bin embeds
  it, two FCs can't bind the same Unix socket). The per-FC
  mount-namespace + bind-mount approach above unblocks N>1; once
  that lands, raise the ceiling. M1.10's `multi_restore` test is
  serial (the warm-pool refill semantic) and passes today;
  `warm_pool_memory` is scaffolded for the un-block PR.
- **gRPC additions**: `LeaseWarmSandbox`, `LaunchWarmSandbox`,
  `ListWarmSlots`. `LeaseWarmResponse` is a oneof of
  `{sandbox_id, StaleTemplate{current_ref}, no_capacity}` so the
  scheduler can distinguish "wrong template ref" from "pool empty".
- **Scheduler change**: `pick_for_session` resolves template_ref. If
  warm-eligible, parallel-ask top-K candidate hosts (by capacity +
  heartbeat-reported warm slots). First non-stale non-empty lease
  wins. `StaleTemplate` errors lazily update coord's known set. All
  no → existing cold-create.
- **Heartbeat extension**: `HostCapacityReport.warm_slots:
  HashMap<TemplateRef, u32>` — coord skips parallel-ask for zero-slot
  hosts. Autoscaler driven by observed lease-success-rate, not
  heartbeat-reported slots (skew-resilient).
- **Autoscaler**: per-template lease/min over a 5-min window;
  N(T) = max(2, ceil(lease_rate × refill_time × 1.2)); floor=0 if 0
  leases in 30 min AND template inactive.

## Milestone 2 — Durability

Goals:

- Sessions in `Active` or `Idle` state survive host loss (graceful
  drain OR ungraceful crash).
- Resume p95 ≤ 1 s (warm-pool-eligible) / ≤ 3 s (cold-restart).
- Lost work bounded by snapshot frequency — default: idle-entry-
  triggered, plus a 5-min wall-clock ceiling.
- Conversation history (PG-backed `session_events`) already survives;
  M2 covers workspace + agent state.

Components:

- **`SessionStatus::Paused`**: snapshot exists, sandbox destroyed,
  system-driven. User clicks Resume; system auto-retries up to N
  times before flipping to Failed.
- **Schema**: `sessions.snapshot_ref UUID`, `snapshots.state_blob_key`,
  `snapshots.sidecar_blob_key`, `snapshots.rootfs_blob_key` (nullable —
  interim writable-rootfs upload), `snapshots.template_ref`.
- **Writable-rootfs interim upload**: `disk_manifest: None` everywhere
  today despite NBD chunked-disk machinery existing (`crates/engram-
  sandbox-firecracker/src/lib.rs:1943, 2383, 2487`). Without uploading
  the writable rootfs, cross-host Resume silently loses edits. M2 v1
  tars + zstd-compresses the writable rootfs and uploads as opaque
  blob. Chunked-disk replacement is a follow-up ADR when the writable-
  NBD plumbing lands.
- **Background snapshot uploader**: per Active sandbox, fires on
  harness-idle entry (existing idle_evictor trigger) AND a 5-min wall-
  clock ceiling. Idle eviction's existing snapshot path becomes the
  durability path (don't destroy after upload). Idempotent: skips if
  no chunk delta since the last `recoverable=true` snapshot.
- **Graceful drain enhancement**: extends `crates/engram-host-agent/
  src/shutdown.rs`. Bump `--shutdown-deadline-secs` default 90 → 180 s
  when graceful-snapshot-upload is enabled. Upload concurrency 4
  (separately bounded from snapshot fan-out 8). Final step: POST
  `/api/hosts/:id/draining` with `(session_id, snapshot_id)` tuples so
  coord can pre-stage Paused transitions if the host dies before its
  terminal log line.
- **Dead-host reaper extension** (`dead_host.rs`): on host death, for
  each Active session, if `snapshot_ref.recoverable=true` → Paused +
  null host_id + null sandbox_id; else Failed (current behavior).
- **Resume API**: `POST /api/sessions/:id/resume`. Validates Paused or
  Idle + recoverable snapshot. Tries warm-pool-eligible templates
  first; for sessions with rootfs delta, cold-restart via
  `host.restore(snapshot_metadata)`. Fresh BootstrapLaunch on restore.
- **SPA UX**: Paused renders with "Last active <duration> ago" + Resume
  button.

## Files modified (overview)

Full breakdown in `~/.claude/plans/cheeky-booping-castle.md`. High-level:

- `crates/engram-sandbox-firecracker/`: path canonicalization in
  snapshot/restore.
- `crates/engram-host-agent/`: new `warm_pool.rs`, new
  `snapshot_uploader.rs`, edits to `shutdown.rs` for upload pipeline +
  extended drain deadline, edits to `idle_evictor.rs` for durability
  path.
- `crates/engram-coordinator/`: new `templates.rs`, new
  `api/resume.rs`, edits to `host_registry.rs` for warm-lease, edits
  to `dead_host.rs` for Paused transition.
- `crates/engram-image-builder/`: post-canonical-capture state.bin +
  sidecar upload.
- `crates/engram-protocol/`: proto additions for warm-pool RPCs;
  HostCapacityReport.warm_slots.
- `crates/engram-core/`: SessionStatus::Paused; trait surface for
  LeaseWarm/LaunchWarm.
- `deploy/migrations/`: 0025 (templates), 0026 (sessions.snapshot_ref),
  0027 (snapshots portable columns).

## Risks

1. **Path canonicalization breaks legacy in-flight snapshots.** Any
   sandbox running today has a non-conformant jail path; its in-memory
   snapshot would fail validation. M1 only produces canonical paths
   going forward; pre-M1 snapshots stay restorable on the same host
   (legacy path). Drain semantics in `dead_host.rs` already mark them
   Failed on host loss, which is the existing behavior — no
   regression.
2. **Canonical-mmap cgroup accounting unvalidated at scale.** Mitigated
   by gating bench (`warm_pool_memory`) before prod rollout. If
   diverged, fallback is conservative depth cap + documented in the
   ADR.
3. **Heartbeat / template-ref skew under load.** `LeaseWarmSandbox`
   returns typed `StaleTemplate{current_ref}` so scheduler distinguishes
   stale-pool-host from no-capacity-host. Hosts keep old-ref slots alive
   60 s after a new ref appears.
4. **Snapshot during active agent I/O.** Substrate-only semantics
   forbids it. Background snapshots only fire on idle-entry. Drain-
   time snapshots accept the I/O-disruption cost because the
   alternative (lose all work) is worse.
5. **Resume vs. fresh-start UX honesty.** Document the contract: Resume
   restarts the agent against persisted state; no live freeze-thaw
   promise. SPA copy reflects this.
6. **Full snapshots only.** FC `client.rs:162` hardcodes
   `SnapshotType::Full`; M2's 5-min wall-clock ceiling rewrites full
   memory.bin every time. Diff snapshots are a follow-up ADR if
   measured bandwidth becomes a problem.
7. **MIG drain deadline tightness.** Existing 90 s budget + new
   per-sandbox upload (~6 s memory + ~3-5 s rootfs at 4× concurrency)
   → ~75 s p99 for a 10-sandbox host. 180 s deadline gives 2× headroom.

## Verification

M1 acceptance gates:

- `cargo test -p engram-coordinator --test warm_pool_integration` —
  fake coord + 2 host-agents; warm pool fills to N=2; 100-session p99
  ≤ 250 ms.
- `cargo test -p engram-sandbox-firecracker --test multi_restore --
  --ignored --nocapture` (dev-vm) — N=5 restores from one canonical;
  independent guest_ip; no cross-talk.
- `cargo bench -p engram-host-agent --bench warm_pool_memory` (dev-vm)
  — N=20 sandboxes; MemAvailable within 10% of canonical-shared total.
- Manual rollout: engrams-internal with warm-pool depth N=1; 24 h soak;
  bump to N=2.

M2 acceptance gates:

- `cargo test -p engram-coordinator --test resume_integration` — host
  A dies mid-session; session → Paused; Resume restores on host B with
  byte-identical workspace.
- `cargo test -p engram-host-agent --test graceful_drain_upload` (dev-vm)
  — 5 sandboxes; SIGTERM; all 5 upload + Paused-eligible within 180 s.
- Manual rollout: engrams-internal force-roll MIG; all active sessions
  → Paused (not Failed); Resume restores user workspace files.

## Open questions / deferred

- **Diff snapshots** if M2 background-upload bandwidth becomes a
  problem.
- **Chunked writable disk** replaces M2's interim tar+zstd upload when
  the writable-NBD plumbing lands.
- **Workspace late-bind for git templates** via virtio-fs lets git
  templates also benefit from warm pool. Future ADR.
- **KSM as fallback** if cgroup-accounting validation forces it. Currently
  rejected for side-channel reasons.
- **Egress policy re-bind on cross-host Resume** — confirm
  `engram-egress-proxy` re-attaches when a sandbox restores on a host
  with a fresh TAP. Trace in the cross-host restore test.

ADR 0012 ("warm pool deferred") and ADR 0009 ("graceful preemption
deferred") both move from `deferred` → `landed via 0014` after M2.

## Related

- ADR 0007: chunked immutable storage — provides the chunked-memory
  substrate this ADR consumes.
- ADR 0009: state reconciliation — extends the dead-host reaper here.
- ADR 0011: HostClient trait — extends the trait surface here.
- ADR 0012: pull-based host dispatch — superseded by this ADR.
- ADR 0013: stateless transport — enables the per-session-create
  parallel-ask scheduling pattern this ADR uses.
