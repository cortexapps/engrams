# engrams memory substrate & snapshot/restore: how it works

A reference for the FC guest-memory machinery: how a microVM's RAM is captured,
stored, restored, evicted, resumed, and teleported. Production isolation is
Firecracker on Linux+KVM; the macOS VZ backend exercises the same control plane
but has no memory substrate. Grounding ADRs: **0007** (chunked storage), **0020**
(UFFD Route B), **0022** (File-backend density), **0028** (eviction durability),
**0034** (idle-eviction state machine), **0039** (cache locality), **0045**
(unified memory substrate / live teleport).

> Code pointers below name files + functions/symbols rather than line numbers
> (which drift). Grep the symbol. This doc describes the **current** design after
> the substrate cleanup series (PRs #459–#465); see "Provenance" at the end.

## 1. Lifecycle of a microVM's memory

Guest RAM moves through five regimes, all served from the content-addressed chunk
store (ADR 0007) — never from a raw `memory.bin` on the resume/teleport paths.

- **Enable.** An operator enables an image; the host-agent's image-prefetch
  supervisor (`image_prefetch.rs`, `spawn_supervisor`/`reconcile`) warms the
  **base snapshot's** disk *and* memory manifests onto local NVMe and **pins**
  them (refcounted, LRU-immune). FC images additionally pre-warm the **base-shm
  substrate** (`prewarm_base_shm`) or, with the substrate off, materialize the
  per-template **File memfile** (`materialize_to_file_cached`) — never both
  (mutually exclusive per `effective_restore_mode`). Readiness gates on chunk
  residency (+ memfile existence in File mode) **and**, for substrate
  (memory-bearing) images, on the uffd base dir actually being a mounted
  tmpfs/shmem — a periodic, self-healing `statfs` check in the reconcile loop, so
  a node-prep tmpfs that mounts minutes after the host-agent starts heals on the
  next tick rather than the host advertising ready and then failing minor faults.

- **Cold base-create** (`session.create`, `fresh == true`).
  `effective_restore_mode(true)` (`engram-sandbox-firecracker/src/lib.rs`) picks
  **Uffd-against-base-shm** when `uffd_base_dir` is set (ADR 0045 — the production
  config), else **File** (ADR 0022 density). Under the substrate **no `memory.bin`
  is materialized**; FC maps the per-template base-shm file `MAP_PRIVATE` and the
  UFFD handler serves canonical pages via `UFFDIO_CONTINUE`, divergent pages via
  `UFFDIO_COPY`, and zeroed pages via `UFFDIO_ZEROPAGE`. The memory-chunk prefetch
  is **backgrounded** on every lazy (UFFD) restore — it warms the cache the
  handler faults from, so it is never on the restore critical path; only a true
  File base-create awaits it (to feed the synchronous `memory.bin` rebuild).

- **Idle eviction (capture).** A host scans its hub and nominates idle sessions;
  the coordinator flips `Active → Evicting` (ADR 0034) and a scanner drives
  capture to completion. Capture chunks `memory.bin` into the store, uploads
  `state.bin` + sidecar to BlobStorage, **then** records the PG snapshot row
  (artifacts durable *before* the row; PG transitions + `commit_snapshot` happen
  *before* `unbind`/`destroy`). Diff-chain checkpoints (ADR 0028) emit
  `SnapshotType::Diff`.

- **Resume** (`fresh == false`). Follows `config.restore_mode` (Uffd in prod).
  Walks snapshots newest-first, re-verifying artifacts at point-of-use and
  enforcing rung-1 coherence (a memory-bearing record restores its *own* disk).
  The UFFD handler faults lazily from the chunk cache; the substrate base-shm is
  mapped under the faults. Falls through the checkpoint chain → disk-only cold
  boot → `Dead`.

- **Teleportation** (live, ADR 0045 C2). Post-copy: the source stays
  paused-and-alive serving guest RAM over a P2P page channel (`process_vm_readv`)
  until drain completes or the TTL expires; the destination restore is forced
  **Uffd** by a staged migration manifest, serves memory lazily from the peer,
  and reads no `memory.bin`. Post-copy requires the substrate on the **source**
  (a File-mode source falls back to snapshot-rehome). Durability is deliberately
  *not* migration's job — the destination rides the periodic checkpoint cadence;
  the finalize is `drain_wait → MigrationCommit` (no post-move Full snapshot, no
  migration-authored row).

**Moving parts:** base snapshot (per-image canonical disk + memory manifests) ·
base-shm substrate (per-image tmpfs file, one host copy, `UFFDIO_CONTINUE`) ·
File vs UFFD restore modes · NBD disk (stock virtio-block + `PATCH /drives`,
host-agent-driven) · chunk cache (NVMe L1, verify-on-populate / trust-on-read) ·
working-set traces (REAP-style first-fault prefault).

## 2. Subsystems

### 2.1 Restore-mode selection
Two host knobs feed `FirecrackerConfig`: `restore_mode` (`ENGRAM_FC_RESTORE_MODE`,
default `Uffd`) and `uffd_base_dir` (`ENGRAM_FC_UFFD_BASE_DIR`, the ADR 0045
substrate). The single authority is `effective_restore_mode(fresh)`: fresh ⇒
`Uffd` iff the substrate is on, else `File`; resume ⇒ `config.restore_mode`. A
migration receive is forced `Uffd` (keyed on a staged `migration-session-manifest.json`),
with no `memory.bin`. `restore_in_jail` requires `memory.bin` to exist **only** in
File mode and spawns the UFFD handler **only** for Uffd. The retired
`ENGRAM_FC_BASE_RESTORE_MODE` knob is fully derived (no stale read).

The host-agent's restore (`pooled_backend.rs`, `restore_with`) computes a single
`memory_is_lazy` predicate = `restore_memory_is_lazy_for(fresh)` **OR** a staged
migration restore (mirroring `restore_in_jail`'s forced-Uffd), and keys **both**
the prefetch-block gate and the `memory.bin`-materialize gate on it. They share
one predicate so they cannot drift from each other or from the load decision: a
lazy restore backgrounds the prefetch and skips materialize; a non-lazy File
restore awaits the prefetch and materializes.

### 2.2 base-shm substrate (ADR 0045)
One physical, page-cache-resident copy of an image's canonical guest memory per
host, shared by every same-template VM via `UFFDIO_CONTINUE` over a `MAP_PRIVATE`
shmem file. Path authority is the single function `uffd_base_path_in(dir, ref)` →
`{manifest_id}-v{version}.base`; three producers (handler `--base-shm`, FC
`uffd_base_file`, image-prefetch pre-warm) must resolve identically for the
canonical ref `base_memory_manifest ?? manifest.memory_manifest`. `BaseShm::open`
grows the file (only) to the canonical `total_bytes` **before** the handler binds
the FC UDS; FC's `O_RDONLY` + `mmap` is ordered after `wait_for_socket`. Only
canonical bytes are ever written into the base file (idempotent across siblings);
divergent / sealed content installs privately. `base_shm_gc::sweep` deletes a file
iff its mtime is older than 30 min **and** no `/proc/*/fd` holds it open. The base
dir **must** be a mounted tmpfs/shmem — `UFFDIO_REGISTER MINOR` is shmem-only
(enforced by the readiness gate in §1).

### 2.3 UFFD handler (ADR 0020 Route B)
A per-VM companion serving every fault from the chunk store, with no `memory.bin`.
`ChunkedMemoryBackend::resolve(offset)` (`chunked.rs`) classifies a chunk-aligned
offset into one of three `ResolvedPage`s:

- **`Canonical`** — the session agrees with the base here (`canonical == session`).
  Install the canonical hash, shared via the base-shm `UFFDIO_CONTINUE` (else
  fetch + COPY).
- **`Chunk { hash }`** — the session diverged / wrote over this chunk; fetch its
  own hash and private-COPY it.
- **`Zero { offset }`** — the session **omits** this offset. The session manifest
  is a *full, zero-omitted* capture (see `chunk_file` and the sparse re-chunk
  path), so an omission is an authoritative "all zero here" — install
  `UFFDIO_ZEROPAGE`, **regardless of what the base holds there**. (Serving the
  base's bytes for a chunk the guest zeroed would silently un-zero guest RAM on
  resume — the bug fixed in #459; the `Canonical` arm's no-hash branch is now a
  defensive fallback.)

A tri-state `installed` bitmap + `Condvar` makes failed installs retryable, not
poisoned; `install_spanning` crosses FC region boundaries; `install_zero_substrate`
latches a COPY-of-zeros fallback if the kernel rejects `ZEROPAGE` on the private
mapping. Prefault is a **background producer** (ADR 0043 P1): peer drain
(hot-first) → trace prefault → `sweep_all`. The handler **writes** only per-host
traces (`publish_trace` → `traces/<mid>/<host>.json`).

### 2.4 Working-set traces (ADR 0014 / ADR 0039)
Traces are REAP-style first-fault recordings used to prefault on a later restore.
**Only the per-host path is live**: the UFFD handler records faults and publishes
`traces/<mid>/<host>.json` (`TraceRef::host`), and the FC spawn replays a host's
prior trace via `--prefault-trace <host>` (and the migration `hot_chunks` rider
via `--prefault-trace file:<path>`).

A second, **canonical** trace (`traces/<mid>/canonical.json`, intended to narrow
the host's base-restore prefetch) was specced but **never built** — no producer
ever wrote it. The dead consumer surface (the `Canonical` prefault spec,
`base_working_set_blob_key`, the `snapshots/<id>/working_set.json` key) was removed
in #464; base restore uses a full-manifest prefetch (backgrounded for lazy
restores per §2.1). If REAP-style base narrowing is wanted, it needs a producer
(a profile pass at enable/bake time that writes the canonical trace) — see
"Future work."

### 2.5 Image-prefetch + pinning (ADR 0015 / 0039)
`spawn_supervisor` owns a `watch` of enabled images; `reconcile` runs the
disable-delta (refcounted `unpin_all` + memfile unlink), an honest-readiness
re-verify (every pinned base hash still `contains_on_disk`, else flip not-ready),
and the enable-delta (`prefetch_one` → `pin_all` → `mark_ready`). `prefetch_one`
warms disk **and** memory manifests from the base snapshot (never OCI), then
either pre-warms the substrate base-shm (best-effort) or builds the File memfile,
and — for substrate images — withholds readiness until the base dir is a mounted
tmpfs (§1). The supervisor's `ChunkStore` uses the default `BlobStorageResolver`
(NVMe → BlobStorage/GCS); the `TieredChunkResolver` + OCI fallback is installed
only on the per-restore chunked-OCI store, not here.

### 2.6 Teleportation (ADR 0045 C2)
Coordinator `migrate_session_live` (`live_migration.rs`): preconditions (no freeze)
→ presetup → spawn the dest restore concurrently → arm the parachute
(`Evacuating`) → **blackout** (pause → disk coherence → fork-v3 vmstate-only →
pagemap seal → register export + peer) → await dest → committing persist
(`rebind_session_guarded`, a single guarded UPDATE) → detached finalize
(`drain_wait → MigrationCommit`; no checkpoint, no row). The source page server
(`migrate_peer.rs`) serves `NeedAt` from the paused source via `process_vm_readv`;
the source FC + handler stay alive until DrainDone/TTL. The dest forces Uffd; its
fetch poller writes `state.bin` **last** (after disk overlay + `BLKFLSBUF`), and
the FC load gate emits `postcopy-never-loaded` on timeout (zero-loss abort).

### 2.7 Idle lifecycle (PG-authoritative)
`sessions.{status, host_id, sandbox_id}` is the only routing authority. The
per-session `session_lease` row (`INSERT … ON CONFLICT` + `DELETE`-on-Drop)
serializes resume / eviction / snapshot / queue / migration, with a 60s heartbeat
vs a 180s reaper and `touch_checked` Lost-vs-Transient classification. Capture
makes artifacts durable in BlobStorage *before* the PG row; snapshot blobs are
GC-governed (`snapshot_blob_pin_set = SELECT id FROM snapshots`, regardless of
`recoverable`). `ensure_active` resumes only `Idle` inline; `Evacuating` returns a
retryable "relocating" 409 and is driven back to `Active` asynchronously by the
`evac_resumer` (it is **not** inline-resumable — `resume_session` has no
`Evacuating` arm and `resume_from_idle`'s rebind CAS is `Idle`-only).

### 2.8 The Firecracker fork (`engram/live-migration`)
Upstream FC v1.16.0 + a small set of commits: the v2 `uffd_base_file` +
`MISSING|MINOR` substrate (ADR 0045 D1) and the v3 `vmstate_only` live path (C2).
A superseded v1 `MAP_SHARED` + `Msync` surface remains vendored but has no host
emitter (`client.rs` has no `shared` field; `SnapshotType` is `{Full, Diff}`) —
slated for retirement. **Invariant:** never edit `src/vmm/src/snapshot/`, never
bump `SNAPSHOT_VERSION` — the fork stays byte-compatible with stock (proven by
`stock_fork_snapshot_compat.rs`), and new fork capabilities are
`skip_serializing_if`-gated so absent fields are byte-identical to stock (which
`deny_unknown_fields`). There is no drive/NBD delta in the fork; rootfs late-bind
rides stock `PATCH /drives`.

### 2.9 Chunk cache & store (ADR 0007 / 0008 / 0021)
`ChunkStore` is the front door (versioned manifests, a `ChunkResolver` seam) over
`BlobStorage`; `ChunkCache` is the NVMe L1. **Verify-on-populate, trust-on-read**:
a chunk's hash is verified exactly once when it's written; reads never re-hash and
never touch mtime/atime; atomic temp + rename gives torn-free visibility.
Singleflight (`LeaderGuard`) dedups concurrent fetches of the same hash, cancel-safe.
`prefetch` confirms residency with a cheap `contains` stat rather than reading
(and discarding) the whole chunk. Eviction is **FIFO by populate time** (oldest
first-write, by mtime) under a free-space floor (ADR 0060: keep ~20% free,
re-probed via `statvfs` every sweep; default no absolute ceiling, optional
`ENGRAM_CHUNK_CACHE_BUDGET_BYTES`) — *not* access-LRU; the hot set is protected
explicitly by the refcounted pin set, not by recency. Unreferenced BlobStorage
chunks are reclaimed by the coordinator's ADR 0016 Phase C GC (`chunk_gc.rs`).

## 3. Cross-cutting invariants

1. **Materialize ≡ load.** `effective_restore_mode(fresh)` and
   `restore_memory_is_lazy_for(fresh)` must stay in agreement; the host-agent
   skips `materialize_memory_if_missing` iff lazy. Both the prefetch-block and
   materialize gates key on one `memory_is_lazy` predicate (substrate- and
   migration-aware) so they cannot drift.
2. **`memory.bin` required only in File mode.** UFFD / migration restores must not
   require it.
3. **Base path single-sourced** through `uffd_base_path_in`; handler, FC load, and
   pre-warm resolve identically for `base_memory_manifest ?? manifest.memory_manifest`.
4. **Base shm holds only canonical bytes**, grow-only, sized before the handler
   binds the UDS, opened `O_RDONLY` + `MAP_PRIVATE`; divergent / sealed content
   installs privately.
5. **Lazy memory never blocks restore.** Only a true File base-create needs the
   memory prefetch on the critical path.
6. **Absence == zero-fill** in the full, zero-omitted session manifest — across
   `chunk_file`, the sparse re-chunk path, and the resolver (`ResolvedPage::Zero`).
7. **Content-addressed trust:** verify the hash exactly once on populate; reads
   never re-hash and never touch mtime/atime; visibility is via atomic temp + rename.
8. **PG is the routing authority;** the per-session lease serializes all lifecycle
   verbs; artifacts are durable before the row; `commit` / PG-transition before
   `destroy`.
9. **Source-alive teleport:** the source FC + handler stay paused-and-alive until
   DrainDone/TTL; ownership flips atomically with one guarded UPDATE; once
   `state.bin` ships, the source may never self-resume in place.
10. **The FC fork stays snapshot-byte-compatible** (no `snapshot/` edits, no
    `SNAPSHOT_VERSION` bump); new fork capabilities must be reachable from the
    host-agent wire structs or they are dead.

## 4. Future work

- **Canonical working-set trace producer.** The base-restore prefetch is
  full-manifest; a profile pass (at enable-time base-snapshot capture, or bake)
  that records the first-fault set and writes `traces/<mid>/canonical.json` would
  let the host narrow it REAP-style. The consumer surface was removed as dead
  (#464); reintroduce it alongside a real producer if pursued.
- **Retire the FC fork's v1 `MAP_SHARED`/`Msync` surface** (ADR 0045 D1) — vendored
  but unreachable from the host-agent.

## Provenance

Synthesized from a multi-agent deep-dive of the substrate / restore / migration /
idle-lifecycle code, then reconciled with the cleanup series it produced:
- #459 — UFFD `resolve()` zero-fill correctness (`ResolvedPage::Zero`).
- #460 — unified substrate+migration-aware `memory_is_lazy` gate (fixes a
  cold-boot regression where a substrate base-create blocked on a full
  memory-manifest re-read).
- #461 — `ensure_active` returns a retryable 409 for `Evacuating`.
- #462 — chunk-cache: `EvictedRing` fix, prefetch residency-stat, doc corrections.
- #463 — substrate image readiness gated on a mounted tmpfs base dir.
- #464 — removed the dead canonical-trace path + adjacent dead code.
- #465 — stale-comment / doc sweep.
