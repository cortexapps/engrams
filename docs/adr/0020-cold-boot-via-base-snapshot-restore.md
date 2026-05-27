# ADR 0020: Cold-boot via base-snapshot restore — 25 s → ~100 ms

Status: 2026-05-27 — **Proposed.** ADR 0019 closed the measurement phase
and ranked **H1 (restore-from-base)** as the lever. This ADR commits to the
optimization: route every cold `POST /sessions` through the existing,
prod-validated `restore()` path against a per-image base snapshot, then shave
the ~1 s restore tail to ~100 ms. Phased P1–P5; each phase lands, is profiled
on the dev-vm, deployed to prod, and **measured against the ADR 0019 trace
spans before the next phase starts**. Flips to Accepted only after the final
phase is prod-measured.

Phase: 1 (P1 = the lever). Commit chain recorded here as work lands.

## Context

ADR 0019 instrumented distributed tracing across all four processes and
produced a measured span→ms breakdown of a real prod cold boot (session
`42725294`, trace `1773e08a…`, freshly-rolled host, ~24 s total):

| phase | dur | note |
|---|---|---|
| `image_resolve` + `materialize` (unspanned gap) | ~7.5 s | cold OCI pull + disk stage |
| `fc.create_in_jail*` | ~0.16 s | host VM setup |
| `fc.await_agent_ready` | ~15 s | kernel boot + ext4 mount of `/dev/vda` + `/dev/vdb` |
| └ **162 serial `chunk.fetch` @ ~84 ms ≈ 13.6 s** | | on-demand NBD page-in, no prefetch/parallelism |
| `fc.spawn_harness` | ~1 s | |

**Every cold session does a full fresh kernel boot.** That is the entire
15–25 s. The dominant sub-cost is 162 strictly-serial GCS chunk fetches paging
the rootfs in on demand during the ext4 mount.

E2B (studied at `~/test/infra`) reaches ~100 ms because it *never* cold-boots:
every sandbox start is `LoadSnapshot` + `ResumeVM` from one per-template
Firecracker snapshot (frozen kernel + guest memory + a running in-VM daemon),
with memory served lazily through a userfaultfd (UFFD) handler guided by a
recorded page-access prefetch trace, a copy-on-write rootfs overlay, and a
pre-allocated network-slot pool. The mechanism, per E2B's
`packages/orchestrator/pkg/sandbox/`:
`fc/client.go` `loadSnapshot()` (UFFD memory backend) → `ResumeVM`; `uffd/`
serves faults from the memfile; `network/pool.go` hands out pre-built slots.

## What engrams already has (verified against the tree, 2026-05-27)

The restore primitive is **already built and prod-validated at ~1 s** on the
idle-resume path (ADR 0014/0018). Contrary to ADR 0014's aspirational text,
`warm_pool.rs` does **not** exist — there is no warm free-list, and the lever
is the existing `restore()` path, not a warm pool. Confirmed present:

- Coord resume path: `restore_for_session` (`crates/engram-coordinator/src/
  host_registry.rs:505`), `resume_from_fc_snapshot` /
  `finish_resume_to_active` (`crates/engram-coordinator/src/api/snapshot.rs`).
- Host restore + **8-wide parallel prefetch with working-set narrowing**:
  `PooledBackend::restore` + `prefetch_memory_chunks`
  (`crates/engram-host-agent/src/pooled_backend.rs:569/589`), reading
  `SnapshotMetadata.working_set_blob_key`. ADR 0014's "queued" M1.13/M1.14
  *consumers* are already in code — just dead, because nothing populates the
  key.
- FC option-D primitives: `patch_drive` / `load_snapshot_paused`
  (`crates/engram-sandbox-firecracker/src/client.rs:119/233`),
  `swap_harness_drive` (`lib.rs:3069`), `reserve_restored_netns`
  (`lib.rs:1927`), `restore_canonical_symlinks` (`lib.rs:1666`).
- UFFD lazy-memory + trace replay: `WorkingSetRecorder`, `prefault_from_trace`
  (`crates/engram-uffd-handler/src/runtime.rs`).
- Option-D mechanism regression test on the dev-vm:
  `crates/engram-sandbox-firecracker/tests/patch_drive_swap.rs`.

**The actual gaps** (what this ADR builds):

1. The image-builder has **no canonical-capture step** — base snapshots are
   never produced (`CanonicalCapture` lives only in the FC test, not the
   builder).
2. The `templates` table was added (`0025`) then **dropped (`0030`)**; there
   is no `base_snapshots` replacement.
3. `create_session_inner` hardcodes `prefer_snapshot_id: None`
   (`crates/engram-coordinator/src/api/sessions.rs:636`) — the create path
   never restores.
4. The UFFD handler still mmaps `memory.bin` with `MAP_POPULATE`
   (`runtime.rs:~288`), synchronously faulting the whole file on the restore
   critical path.
5. Per-fork identity hazards (shared `/etc/machine-id`, `/dev/urandom` seed,
   clock) are unaddressed — N restores from one base are clones.

## Decision

Make cold create call the already-working `restore()` path against a per-image
base snapshot, with cold-create as the fallback. This is a much smaller change
than ADR 0014's warm-pool framing implied, because `restore()` already contains
the prefetch, working-set, netns, and UFFD logic. Then close the ~1 s → ~100 ms
tail with lazy memory + bake-time prefetch trace, restore-setup parallelism,
and a network-slot pool. Phases land and are measured one at a time.

## Phase 1 — Route cold create through base-snapshot restore [15 s → ~1 s]

The single highest-leverage step: it removes the kernel boot, both ext4 mounts,
and 100 % of the 13.6 s serial page-in in one move.

- **1a. Bake-time base snapshot per image** (`crates/engram-image-builder/`).
  Port the canonical-capture flow from `patch_drive_swap.rs` into the builder:
  boot the baked rootfs under FC with the **stub harness** attached (option-D
  capture point — bootstrap on `accept()`, harness *unmounted*) → pause →
  full FC snapshot (`SnapshotType::Full`, the only mode wired in `client.rs`)
  → chunk `memory.bin` into the chunk store → upload `state.bin` + sidecar JSON
  to `snapshots/<id>/{state.bin,sidecar.json}` → emit a `canonical_snapshot`
  block in `bundle.json`. The bake **must** use the canonical `<work_dir>`
  (`/var/lib/engram/sandboxes`) so the `state.bin`-embedded vsock/rootfs paths
  match what `restore_canonical_symlinks` recreates on the receiver; fail-fast
  at capture on non-conformant paths.
- **1b. `base_snapshots` table + enable cascade.** New migration
  `deploy/migrations/0031_base_snapshots.sql`:
  `base_snapshots(manifest_digest TEXT PK, snapshot_id UUID → snapshots(id),
  image_repo, image_tag, vcpus, memory_mib, created_at)`. In
  `crates/engram-coordinator/src/api/enabled_images.rs`: when the bundle carries
  `canonical_snapshot`, insert a `snapshots` row (`session_id=NULL`, supported
  since `0028`; `recoverable=true`) + a `base_snapshots` row in one PG
  transaction. Idempotent on re-enable. This is ADR 0014's M1.11 cascade
  retargeted from the dropped `templates` table to `base_snapshots`. Memory
  chunks are already in BlobStorage from the bake, so first session is a GCS
  pull (intra-VPC), not an OCI pull.
- **1c. Branch the create path into restore** (`api/sessions.rs`,
  `create_session_inner` ~626–674). Look up `base_snapshots` by
  `enabled.manifest_digest`. On hit: build a `SnapshotMetadata` like
  `resume_from_fc_snapshot`, set `prefer_snapshot_id`, call `restore_for_session`
  instead of `create_for_session`, then reuse `finish_resume_to_active` for
  post-restore identity binding + egress + `start_agent`'s `SpawnHarness` RPC
  (per-session harness late-bound via `swap_harness_drive`/`patch_drive`). On
  miss/error: fall through to the existing cold `create_for_session` with one
  `tracing::warn` — mandatory fallback for older images.
- **1d. Per-fork hazards (correctness gate).** Before harness spawn, reseed
  `/dev/urandom`, step the clock, regenerate `/etc/machine-id`. Every restore
  from one base shares baked entropy/identity; this is the most likely
  correctness landmine.
- **1e. Serialize restores (depth-1).** Concurrent restores from one base
  collide on the `state.bin` vsock-UDS path (`EADDRINUSE`; ADR 0014 risk 9,
  hotfix `34b18aa`). Add a per-`manifest_digest` inflight guard (atomic
  `DashMap` `Entry`). Correct but caps throughput; P5 lifts it.

**Measure:** the 162 `chunk.fetch` spans and the 15 s `fc.await_agent_ready`
vanish from the create trace, which now routes
`coord.restore_for_session → restore_in_jail → uffd.run`.
`engram_session_boot_seconds{kind=cold}` collapses toward the warm ~1 s. The
~7.5 s `image_resolve`/`materialize` gap disappears (memory from chunks, no OCI
rootfs stage).

## Phase 2 — Lazy memory + bake-time working-set prefetch [~1 s → ~300–400 ms]

- **2a.** `runtime.rs:~288`: `MAP_PRIVATE | MAP_POPULATE` → `MAP_PRIVATE` +
  targeted `MADV_WILLNEED` over the working-set pages (E2B's two-phase
  fetch→copy). The `uffd.map_populate` span already measures this.
- **2b.** `engram-image-builder`: after 1a's capture, restore with
  `WorkingSetRecorder` active, run the **synthetic mount+exec** (push
  `BootstrapLaunch{argv=/bin/true, harness_dev=/dev/vdb}` so the kernel touches
  the ext4-mount + execve pages the real warm-lease will touch), settle ~3–5 s,
  serialize the trace, upload, set `SnapshotMetadata.working_set_blob_key`. **No
  host change needed** — `prefetch_memory_chunks` + `prefault_from_trace`
  already consume the key; this lights up dead code (the cheapest high-value
  change in the plan).
- **2c.** The serial-fetch problem is already solved on restore (8-wide
  parallel + working-set-narrowed); P1 removed the ext4-mount NBD page-in.

**Measure:** `uffd.map_populate` → near-zero;
`engram_chunk_fetch_seconds{tier=blobstorage}` count drops on 2nd+ restore per
host; `{kind=cold}` p50 under ~500 ms.

## Phase 3 — Parallelize restore subsystem setup [~300 ms → ~150 ms]

`restore_in_jail` (`lib.rs:1604`) + `PooledBackend::restore`
(`pooled_backend.rs:1723`) run netns → FC spawn → symlinks → UFFD spawn →
socket wait → materialize → load_snapshot serially. `try_join!` the independent
units, join at the `load_snapshot_paused` gate; rework partial-init cleanup for
the joined shape. Add the deferred `restore_in_jail` sub-step spans (ADR 0019
0c) to measure the collapse to the max leg.

## Phase 4 — Network slot pre-allocation pool [~150 ms → ~100 ms]

`crates/engram-sandbox-firecracker/src/net.rs` + `reserve_restored_netns`
(`lib.rs:1927`): pre-create a pool of netns+TAP+SNAT slots (E2B
`network/pool.go` shape, e.g. New=32 / Reused=100); restore pops a ready slot
and rebinds the bake's TAP name inside it; background refill keeps it topped.
Low yield (~30 ms), the last leg to ~100 ms.

## Phase 5 — N>1 concurrent restores per base (throughput, not latency)

`spawn_firecracker` `pre_exec`: `unshare(CLONE_NEWNS)` + per-FC bind-mount of
the canonical rootfs/vsock paths to private copies (the per-FC mount-namespace
approach ADR 0014 defers). Lifts P1's depth-1 cap. Sequence last, only if
measured demand requires concurrent restores per image per host.

## Per-phase ship loop

Each phase: implement → per-crate `cargo test -p <crate>` → `just check` clean
locally + dev-vm clippy for any Linux-gated path → **manually exercise +
profile on the dev-vm** (drive a real cold-boot `POST /sessions`, capture the
local Jaeger trace via `just integration-up`, confirm correctness *and* the
speedup) → push to `main` → drive a fresh cold boot in prod via
`engrams-prod-ops` → confirm the phase target from Cloud Trace +
`engram_session_boot_seconds{kind=cold}` → record the result here → next phase.
Do **not** batch phases.

## Risks

- **Per-fork identity** (machine-id / urandom / clock) — handle in 1d before
  harness spawn, or every restored session is an entropy clone.
- **vsock-UDS `EADDRINUSE`** on concurrent restore — serialized in 1e, fully
  fixed in P5's mount namespace.
- **Path-canonicalization conformance** — the bake (1a) must use the canonical
  `<work_dir>` or `restore_canonical_symlinks` fails on the receiver; fail-fast
  at capture.
- **Full snapshots only** (`client.rs` hardcodes `SnapshotType::Full`) — fine
  for one base per image; no diff needed.
- **Cold-OCI tail** (15 s→30 s on freshly-rolled hosts) — P1 sources the rootfs
  from chunked `memory.bin` (GCS, intra-VPC), off the create path; still gate
  "host ready for scheduling" on `image_prefetch` completion.

## References

- ADR 0019 (cold-boot measurement; the trace breakdown this builds on)
- ADR 0014 (portable snapshots, option-D harness late-bind, working-set/prefetch
  design; warm-pool framing superseded here — the restore path is the lever)
- ADR 0018 (session evacuation; §12p vsock/harness path re-anchoring)
- ADR 0007 (chunked immutable storage / canonical-memory UFFD restore)
- E2B infra reference: `~/test/infra/packages/orchestrator/pkg/sandbox/`
