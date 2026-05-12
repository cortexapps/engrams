# ADR 0007: Chunked-immutable storage with content-addressed manifests

Status: accepted (incremental rollout in flight), 2026-05-11
Phase: 7 (production deploy)
Supersedes: the cold-tier flush pipeline of ADR 0005. Snapshots
are no longer two-tier; chunked storage is the single durability
primitive.

## Context

ADR 0005's `tar+zstd → BlobStorage` cold-tier flush got us a
working production-grade durability path, but loses several
operationally-important properties as the deployment grows:

- **Cross-host migration is slow.** Per-session migration =
  pause → tar a multi-GB rootfs → upload → download on the
  target → untar → restore. 30 s+ per session on a healthy
  network. Doesn't fit a 30 s spot-preemption window if a host
  is running 10 sessions.
- **No deduplication.** Two sessions on the same image each
  hold their own tar.zst. Storage cost scales linearly with
  session count rather than image diversity.
- **No cross-VM memory sharing.** Each VM's RAM lives in a
  separate memory.bin file. 1000 Python sessions × 4 GiB
  reservation = 4 TiB even though 99% of the pages are
  identical across them.
- **Restore lags vCPU run-time.** UFFD-from-file restores
  serve faults from a single sequential read; a multi-vCPU VM
  fault-storms during early boot and the handler serialises.

The production references we pulled from (AWS Lambda's
"warm freeze" Firecracker pool, Replit's Goval chunked-disk
substrate, Fly.io's containers, AWS Aurora DSQL's snapshot
layer) all converged on chunked-immutable storage with content-
addressed deduplication as the right primitive. The benefits
they publish — sub-100 ms restore after warm-up, near-free
fork, COW at multiple layers, cross-host portability — are all
things Engram needs.

## Decision

Sandboxes' disk and memory state both live in a **chunk store**:
content-addressed, immutable, in `BlobStorage`. Manifests are
versioned references that point at the chunks.

### Chunk sizes

- **Disk chunks: 16 MiB.** Replit's Goval ships 16 MiB; large
  enough to amortise per-chunk overhead, small enough that
  partial dirty isn't a write of the whole disk. One GCS PUT
  per chunk is the right scale.
- **Memory chunks: 512 KB.** AWS Lambda's REAP-style restore
  uses chunks this size. Small enough to avoid wasted upload
  on partial dirty, large enough to keep the manifest
  manageable for a multi-GB VM.

### Manifest shape

A `ManifestRef = (manifest_id: UUID, version: u64)`. The
manifest_id is stable across the lifetime of a virtual
disk/memory; the version ticks monotonically as the manifest
mutates. Forks reuse the manifest_id at version=parent+1 with
a `parent` back-reference. ChunkHash is sha256.

### Memory dedup across VMs

The image-builder boots the VM once during bake, pauses at
steady state (post-init, pre-work), captures `memory.bin`, and
chunks it. This is the **canonical memory snapshot**. New
sessions `mmap(MAP_PRIVATE, canonical_fd, 0)`; the kernel
serves clean pages from the host page cache (one copy across
all sessions), MMU page-fault handler allocates a private copy
on first write. Hardware-enforced COW; no userspace page
hashing (avoids KSM-style cross-tenant side-channel surface
that AWS + Aurora DSQL explicitly avoid).

Practical impact: 1000 Python sessions each reserving 4 GiB =
4 GiB canonical + 1000 × ~100 MiB per-session deltas ≈ 100 GiB
total, not 4 TiB.

### Working-set traces

First UFFD restore on a host: the handler records every chunk
faulted in during the first 5 s of vCPU run-time. The trace is
persisted per-(manifest, host) at `traces/<manifest_id>/<host>.json`.
Subsequent restores read the trace and prefault the chunks
synchronously before unfreezing vCPUs. ~3.7× speedup per REAP
in published benchmarks.

### Layered architecture

```
Layer 3:  SandboxBackend integration
          - FC: NBD-from-chunks (disk) + UFFD-from-chunks (memory)
          - VZ: materialize-to-file (disk only; memory deferred)
          - Process: chunked rootfs via materialize-to-cwd

Layer 2:  Block / page-device adapters (platform-specific)

Layer 1:  Chunk store (cloud-agnostic, identical everywhere)
          - Manifests (versioned, content-addressed)
          - Chunks (content-hash-addressed, immutable)
          - Local NVMe cache (LRU)
          - GC by manifest reachability
          - Working-set traces

          Backed by BlobStorage (LocalBlobStorage / GcsBlobStorage)
```

### Three free COW levels

1. **Disk COW** — chunks shared by N sessions referencing the
   same image. Writes produce new chunks; per-session manifest
   gets new pointers for dirty offsets. The base manifest never
   changes.
2. **Memory COW** — hardware-enforced via `MAP_PRIVATE` of the
   canonical base.
3. **Session fork COW** — `POST /sessions/:id/fork` is a tiny
   write (a Postgres row + a manifest JSON in BlobStorage). Both
   forks reference the same chunks; they diverge only as either
   writes. Useful for "try N approaches in parallel" and
   time-travel debugging.

### Cold-tier flush deleted

The `tar+zstd → BlobStorage` pipeline (`flush.rs`,
`disk_pressure.rs`, the seal-blob-ref machinery, the cold-tier
columns on `snapshots`) is **retired**. Chunked storage IS the
durability layer; there's no hot/cold dichotomy.

### Wire protocol

`engram-protocol::WIRE_VERSION` is a monotonic constant
exchanged in the dialer's hello frame. Bincode is positional;
adding a field to any serde-derived type at the wire boundary
silently misaligns every byte that follows. The handshake
catches mismatched coord/host-agent versions loudly and refuses
to register.

Version history:
- v1: introduced alongside this ADR (`SnapshotMetadata.disk_manifest` add).
- v2: added `RequestKind::ResolveRegistryAuth` for the
  standalone host-agent's OCI auth path.

## Consequences

- **Disk COW**: free via content-addressed chunks.
- **Memory COW**: hardware-enforced via MAP_PRIVATE.
- **Session fork COW**: O(manifest size) — a few KB.
- **Cross-host migration**: pause → flush dirty chunks → bind
  to target → restore (with WS prefault). ~1–2 s per session.
  Enables MIG rolling deploys + spot/preemption without
  trashing user state.
- **Storage cost**: dedup across sessions is automatic. Base
  image's GB are stored once regardless of session count.
- **Single-tier durability**: no more hot/cold dichotomy.
  Sessions live ↔ chunks reachable in BlobStorage.
- **MacOS dev**: gets disk COW via APFS clonefile + chunked
  disk; memory COW deferred (VZ memory snapshot is broken
  upstream for arm64 anyway).
- **Backward incompatibility**: existing baked images become
  invalid. Greenfield rebuild.
- **OCI auth on standalone host-agent**: the host can't
  instantiate `PgAuthResolver` (no DB/KEK access). The
  `RequestKind::ResolveRegistryAuth` WS-RPC delegates to the
  coord's resolver. Plaintext creds traverse the WS only at
  pull time; never persisted on the host.

## Alternatives considered

- *Keep tar+zstd, add chunked-disk only.* Leaves the memory
  snapshot story (the bigger win) unsolved.
- *Page-level memory chunks (4 KiB).* Manifest explosion —
  ~1M entries per 4 GiB VM. Production systems use
  256 KB – 1 MiB.
- *KSM-style cross-tenant page hashing.* Side-channel surface.
  AWS + Aurora DSQL explicitly avoid; we follow.
- *FC live VM migration.* Not productionally supported by the
  Firecracker project.
- *Coord-issued, hostless auth proxy.* Considered for OCI
  credential delivery to the standalone host-agent. The WS-RPC
  approach keeps creds inside the coord process boundary,
  matches the existing dialer flow, and doesn't introduce a new
  network service to operate.

## Rollout

Tracked separately in `docs/chunked-storage-rollout.md`. Four
maturity tiers:

1. **Mac local (`--mode=all`)** — Done. Tests assert the chunked
   path end-to-end through `PooledBackend`.
2. **Linux dev-vm + FC** — Done. All 7 FC integration tests
   pass against real microVMs using the chunked image-builder.
3. **Coord + host split** — Wiring done (blob from env, chunk
   store, image cache, OCI auth via WS-RPC, wire-version
   handshake). Real-world two-process validation pending.
4. **GCP production** — Helm chart, Packer host image, GCP
   Terraform reference shipped; real `helm install` + `terraform
   apply` against a live project remain operator gates.

## What this ADR does NOT cover

The following are tracked as "still pending" in the rollout doc
and have their own deferred-decision notes:

- **Observability** — no metrics on the chunked path yet.
  Cache hit rate, chunk fetch latency, materialize time, GC
  counters are all silent in production. Needs a framework
  call on whether the rest of the stack adopts Prometheus
  alongside the existing structured-`tracing` logs.
- **Phase 6 destructive trait reshape** — shipped.
  `SandboxBackend::snapshot(id)` (no `dest: &Path`) +
  `restore(metadata: SnapshotMetadata)` (no `src: PathBuf`) +
  `snapshot_path_for(id)` accessor. Backends own their own
  per-snapshot staging dir under `<work_dir>/snapshots/<id>/`.
  WIRE_VERSION bumped to v4 for the
  `RequestKind::{Snapshot, Restore}` shape change.
- **NBD adapter for FC disks** — shipped in this branch
  (`55dd889`, `c770b6f`, `645afd8`, `e4f7500`, `94e9a52`).
  PooledBackend prefers NBD over materialize-to-file when
  `ENGRAM_NBD_DEVICES` is set + the manifest is chunked.
- **UFFD-from-chunks for memory** — shipped (Phase 5). Bake-
  time canonical-memory capture lands `canonical_memory_manifest`
  in `bundle.json`; the UFFD handler resolves session faults via
  the chunk store with MAP_PRIVATE'd canonical base for
  cross-VM page-cache sharing.
- **Migration 0018/0019 schema reshape** — landed in Phase 7
  (commit history; `0020_drop_cold_tier.sql` is the destructive
  cap). The `snapshots` table now carries `disk_manifest_id` +
  `disk_manifest_version` + `memory_manifest_id` +
  `memory_manifest_version` with a CHECK constraint and partial
  GC index.
- **Materialized-rootfs orphan reap + chunk-store GC scheduler**
  — both shipped. `POST /api/admin/reap-materialize-dir` +
  `POST /api/admin/gc-chunks` for explicit triggers; the
  coordinator's `chunk_gc::spawn` is the cron driver (interval
  via `ENGRAM_CHUNK_GC_INTERVAL_SECS`, retain via
  `ENGRAM_CHUNK_GC_RETAIN_SECS`).
