# ADR 0008: Chunks in OCI with tiered cache

Status: accepted (rollout shipped), 2026-05-12
Phase: 5 (all sub-phases shipped; integration-tested in CI)
Supersedes: the storage-substrate decision of ADR 0007.
Chunk sizes, manifest shape, COW levels, and snapshot
semantics are unchanged. What changes is where image chunks
live durably and how chunk faults resolve them.

## Context

ADR 0007 made `BlobStorage` (GCS in prod, local FS in dev)
the single durability primitive for both image chunks and
runtime snapshot chunks. That choice optimized for fleet-
internal economics — one substrate, one cache, one GC sweep
— but traded away a property operators reflexively expect:
**the OCI image URI is not self-contained**.

Today's flow:

- Bake writes chunks to BlobStorage; pushes ~KB of metadata
  to OCI (commit `d79094f` made the rootfs.ext4 layer
  optional and bake skips it when `bundle.json` is present).
- Host pulls the OCI metadata; reads chunks from BlobStorage
  through the manifest pointer.
- If the host's BlobStorage namespace doesn't hold the
  chunks the manifest points at → silent failure at session-
  create time when a chunk faults and 404s.

The artifact bricks across BlobStorage namespaces. Cross-
region, cross-org, and partner-shipped images all fail in
this mode without an out-of-band chunk transfer. The failure
is silent at push and pull; it only surfaces on first chunk
fault. That's a footgun, and it forecloses several product
shapes (partner-baked base images, customer-supplied
workloads, dev-laptop → prod registry handoff).

The OCI ecosystem has a different answer: chunks live as OCI
layers, addressed by content, distributed by the registry.
Two reference implementations:

- **Nydus / RAFS** (`dragonflyoss/nydus`, Rust, CNCF Sandbox)
  — splits an OCI image into a small bootstrap (file
  metadata + chunk pointers) and one or more chunk blobs
  (concatenated 16 MiB chunks). Hosts pull the bootstrap
  eagerly and Range-GET into the chunk blob on fault.
- **SOCI** (`awslabs/soci-snapshotter`, Go) — adds a ztoc
  (zstd-table-of-contents) as an OCI referrer artifact; the
  image stays a conventional tar.gz layer but is internally
  seekable.

Both treat the OCI registry as the durable source of truth
for image bytes. Local caches accelerate, but don't
constitute, the image's identity.

Engram's runtime requires byte-aligned chunking — NBD-from-
chunks, UFFD-from-chunks, and MAP_PRIVATE canonical memory
all depend on chunks landing at predictable byte offsets in
the underlying file. SOCI's tar-offset model can't deliver
that. Nydus's chunk-blob model can.

## Decision

The OCI registry becomes the **durable source of truth** for
image chunks. `BlobStorage` becomes a **regional read-through
cache** for image chunks AND the source of truth for runtime
snapshot chunks. Chunk faults resolve `local NVMe →
BlobStorage cache → OCI registry`, with opportunistic write-
through fill.

### OCI artifact shape

Bake produces a Nydus-shaped OCI artifact:

```
OCI manifest references layers:
  - application/vnd.engram.manifest.v1+toml          ~1 KB
  - application/vnd.engram.bootstrap.disk.v1+json    ~100 KB
  - application/vnd.engram.chunks.disk.v1            ~GB
  - application/vnd.engram.bootstrap.memory.v1+json  ~100 KB (opt)
  - application/vnd.engram.chunks.memory.v1          ~GB     (opt)
```

The bootstraps are small JSON documents:
`Vec<ChunkRef { file_offset, blob_offset, len, sha256 }>`.
They're pulled eagerly on first contact with the image and
cached locally in `image_cache`.

The chunk blobs are concatenations of all chunks in working-
set order. They are **never fully pulled** — chunks are
Range-GETted on demand. 16 MiB chunks for disk; 512 KB chunks
for memory; both disk-aligned at byte offsets in the
underlying file (see §Chunk granularity).

This is the same chunked content the bake produces today.
What changes is the destination: bake writes the artifact to
OCI as layers instead of (or in addition to) BlobStorage as
discrete keys.

### Tiered chunk resolution

Chunk lookup becomes a three-tier fallback:

```
fault → local NVMe cache (per-host LRU, ~200 GiB)
      → BlobStorage (regional cache; may be partial)
      → OCI registry (Range GET into chunks.<kind> blob)
            └─ tee back to BlobStorage on hit (CDN-fill)
```

Each tier miss falls through to the next. The local NVMe
cache (`ChunkCache`, ADR 0007) is unchanged. BlobStorage is
a regional cache that may be partially populated. The OCI
registry is the always-authoritative origin. CDN-fill
semantics keep the BlobStorage cache warm from runtime
traffic without an explicit pre-population step.

The fault path is symmetric for disk (NBD) and memory (UFFD):
both resolve through the same `ChunkResolver` trait, both
fall through the same three tiers. Runtime mechanics differ
(block reads vs page faults, prefault behavior); storage
resolution doesn't.

### Write-through on produce (2026-07-01 amendment)

The read path above populates the local NVMe cache on a miss
(CDN-fill), but the **write** path did not: `ChunkStore::put_chunk`
wrote only to BlobStorage. So a host that *produced* a chunk — most
importantly an **idle-eviction re-chunk of divergent guest memory**
(`update_for_dirty_ranges_sparse` with `local_sink = None`) — shipped
it to GCS and discarded the local copy it already had. Every resume
then re-paged that session's entire divergent working set back from
GCS at ~110 ms/chunk (measured: ~16 faults/s, a resume stuck in
`Created` for minutes), **even on the same host that captured it, and
even though eviction was not the cause** (the cache retains ~18 h of
chunks; LRU keeps the recent set). The teleport path had already
solved exactly this for the pause window with a `Some(cache)` local
sink (ADR 0045 C1); ordinary capture never got it.

Fix: make the write side symmetric with the read side. `ChunkStore`
gains an optional write-through `ChunkCache`; `put_chunk` populates it
after the durable BlobStorage write, via the cache's `write_local`
(the debounced free-floor sweep still enforces the budget — a burst of
write-throughs does **not** skip eviction, so a disk-pressured host
can't overshoot the floor; and `hash` is already known, so no
redundant re-hash). The host-agent wires the same `ChunkCache` its
UFFD handler + disk daemon read from; the coordinator wires none, so
`put_chunk` there stays blob-only. Content-addressing makes this safe
(a write-through copy can never be stale). The read side (`get_chunk`
seeding a re-chunk) is *not* yet cache-tiered — a possible follow-up,
but it's a capture-time cost, not the resume-time cost fixed here.

### Canonical vs per-session

The architectural line is not disk-vs-memory; it's
**canonical-vs-per-session**:

- **Canonical chunks** (disk + memory, immutable, bake-time):
  in OCI as layers. Part of the image identity. Distributed
  by the registry. Fault-resolved through the tiered path.
- **Per-session snapshot chunks** (disk + memory, mutable,
  runtime): BlobStorage only. No OCI home — registries don't
  do append, don't do per-session granularity, don't tolerate
  high write rates.

Cross-source dedup is automatic via content addressing. A
session's clean disk pages dedup against the image's disk
chunks; a session's clean memory pages dedup against the
canonical memory chunks. The same sha256 maps to the same
BlobStorage key regardless of whether the chunk arrived as
an OCI tee-fill or as a snapshot write.

### GC model

All chunks in BlobStorage are equally durable. "Cache" vs
"snapshot" is a population-source label, not a lifetime
distinction. A chunk is evictable iff no live manifest
references its hash — same primitive
`engram_coordinator::chunk_gc` originally ran (removed
2026-05-23; see ADR 0015 M5). Optional LRU eviction *within*
the reachable set bounds cache growth without affecting
correctness.

OCI tag retention governs durable image chunks at the
registry side, as usual.

### Cross-image dedup via base/diff layers

A bake derived from a parent image references the parent's
`chunks.disk` blob as a separate OCI layer, shared at the
registry's natural content-addressing of layer digests. The
child's bootstrap points at chunks across both blobs via
`(blob_digest, blob_offset, len, sha256)` triples.

This preserves the cross-image dedup property today's
BlobStorage gets for free. Image-builder gains lineage
awareness: it consults the parent image's bootstrap at bake
time to identify chunks already in the base layer and writes
only the diff blob.

### Chunk granularity

Disk chunks remain 16 MiB; memory chunks remain 512 KB; both
**disk-aligned** at byte offsets in the underlying file (the
ext4 image for disk, `memory.bin` for memory).

File-level chunking (Nydus-default — hash each file's content
independently) is considered and deferred. Moving to file-
aligned chunks would:

- Require replacing NBD with virtiofs/RAFS on the FC side
  (significant kernel/runtime change).
- Block VZ's materialize-to-file path entirely; VZ doesn't
  support FUSE-mounted disks cleanly.
- Change the guest's view from "block device" to "FUSE
  filesystem" — a category change with its own ADR's worth
  of follow-on work.

The cross-image dedup gain is modest for Engram's current
image diversity (where sibling tags and snapshot-vs-base are
the dominant dedup cases, both of which work fine with disk-
aligned chunking). Revisit if real workloads demonstrate gaps
— concrete migration shape in §Future direction below.

### Supply-chain scanning

Trivy, Grype, Snyk Container, Docker Scout, and `syft` walk
filesystem layers (typically tar.gz) to enumerate packages,
generate SBOMs, and detect CVEs. A `chunks.disk` blob is
neither tar nor gz; these tools cannot read it.

What keeps working unchanged:

- **cosign / sigstore / Notation** — operate at the OCI
  manifest digest level. Signing the chunked artifact is
  unaffected.
- **SLSA provenance attestations** — build-time metadata,
  not artifact-format-dependent.

What needs explicit mitigation:

- **Vuln scanning + SBOM (primary: scan-at-bake).** The bake
  pipeline runs Trivy + `syft` against the staged ext4 (or
  the pre-chunk Docker export). Results attach as OCI
  Referrer artifacts via the OCI 1.1 Referrers API
  (`subject: <chunked-manifest-digest>`). Consumers
  (admission controllers, audit pipelines) read the referrer
  instead of re-scanning. Same pattern SOCI uses for ztoc.
  Adds ~30-60 s per bake when enabled.
- **Fallback: dual-publish for environments without referrer
  support.** Push a conventional OCI image (tar.gz layers)
  under a `<tag>-scan` suffix used only by scanners; the
  chunked artifact under the canonical tag is what runtime
  pulls. Costs 2× registry storage for the image bytes.

### Warm-daemon distribution (downstream benefit)

ADR 0007's `BuildRequest.capture_canonical_memory` primitive
becomes a v1-distributable user feature in this world. Bake
can boot the just-built rootfs, run a user-specified warmup
script (e.g., `gradle --daemon-start`), capture the steady-
state memory snapshot, chunk it, and ship it as the OCI
artifact's `chunks.memory` layer. New sessions inherit the
warmed state via UFFD MAP_PRIVATE — Gradle daemon already
running, JIT-warm, classes loaded.

The user-facing `[bake.warmup]` knob in `engram.toml` is a
separate ADR. This ADR's contribution is the distribution
shape: warm-daemon memory snapshots travel inside the same
OCI artifact as the rootfs, with the same fault behavior and
cross-host portability properties.

### Wire protocol / schema changes

- `WIRE_VERSION` does not bump for the OCI side — host-agent
  doesn't carry chunk-fetch state over WS.
- `ImageBundle` schema bumps to v2: adds
  `bootstrap_layers: Vec<BootstrapRef>` and
  `chunk_blob_layers: Vec<BlobRef>` keyed by OCI layer
  digest. v1 fields (`disk_manifest`,
  `canonical_memory_manifest`) remain readable for ADR-0007
  artifacts so the migration is tolerable.
- New `ChunkResolver` trait in `engram-chunk-store`. Three
  impls:
  - `BlobStorageResolver` — today's behavior; populates the
    cache tier.
  - `OciChunkResolver` — Range-GET against the registry;
    populates the origin tier.
  - `TieredChunkResolver` — composes the two plus the
    existing NVMe `ChunkCache`.
- `engram-oci` gains: Range GET on blob endpoints, OCI 1.1
  Referrers API client (push + pull subject-referenced
  artifacts).
- BlobStorage path scheme adds an OCI-digest index:
  `bundles/<oci-manifest-digest>.json` carrying the parsed
  bootstrap-layer digests. Lets a second host in the same
  namespace skip re-parsing OCI layers. ~1 KB per image;
  serves as the singleflight checkpoint for concurrent first-
  pulls.

## Consequences

- **Self-contained OCI artifact.** ✅ Image URI is portable
  across BlobStorage namespaces. Cross-region, cross-org,
  partner-shipped images all work without out-of-band chunk
  transfer.
- **Cross-image dedup preserved at the durable layer.** ✅
  Via base/diff OCI layers. Requires bake-time lineage logic.
- **BlobStorage becomes a derived cache for image chunks.**
  ✅ Losing the namespace degrades performance but doesn't
  lose image data. Snapshot chunks still need BlobStorage
  for durability.
- **Snapshot path unchanged.** ✅ `PooledBackend::snapshot`,
  the NBD flush, UFFD's memory-chunking path, and
  `traces/<manifest_id>/<host>.json` all continue to operate
  against BlobStorage as today. *(The `chunk_gc` reachability
  sweep that previously rounded this out was removed 2026-05-23
  — see ADR 0015 M5.)*
- **First-fault cost on a fresh region.** ⚠
  Measured estimates: ~250-600 ms cold-boot wall (same-region
  registry, WS-trace prefault); ~3-5 s cross-region;
  ~5-15 s on public registries without chunk coalescing.
  Working-set traces keep memory-resume under 1 s same-
  region.
- **Range GET fragility across registries.** ⚠
  Behavior known good on ECR / GAR / GHCR / Harbor. Docker
  Hub and self-hosted `distribution` registries vary; some
  throttle high-frequency Range traffic. Per-registry
  validation gated in rollout.
- **Supply-chain tooling regression without mitigation.** ⚠
  Trivy / Grype / Snyk / Docker Scout / syft break unless
  the scan-at-bake + referrer-artifact mitigation lands.
  Adds ~30-60 s to bake when enabled.
- **GC story spans two stores.** ⚠
  OCI tag retention governs image chunks at the registry
  side; BlobStorage GC governs cache fills + snapshot writes.
  Operators tune retention separately on each.
- **Engineering cost.** ~6-9 weeks for one focused engineer:
  Range-GET + Referrers in `engram-oci` (~1-2 weeks);
  `ChunkResolver` trait + tiered impl (~1 week); image_cache
  rewrite for v2 bundle schema (~1 week); Nydus-shaped output
  + base/diff layer logic in image-builder (~2 weeks); scan +
  SBOM + referrer publish (~1 week); per-registry
  compatibility validation (~2-3 weeks elapsed, bursty).

## Alternatives considered

- **Status quo (ADR 0007 as-is).** Rejected if image URI
  portability matters. Defensible if Engram commits to a
  closed fleet forever.
- **Lazy-explode of self-contained ext4 OCI artifacts.**
  Revert commit `d79094f`'s layer-skip; add a chunk-on-pull-
  miss helper on the host that re-chunks the OCI ext4 into
  BlobStorage. Strictly weaker than this proposal —
  doesn't get Range-GET-on-fault, doesn't dedup at OCI
  layer granularity, doesn't recover canonical-memory
  portability — but a defensible intermediate. Worth keeping
  as the migration's first phase before the full Nydus-
  shaped artifact ships (see Rollout §2).
- **Pure Nydus (no BlobStorage at all).** Rejected. Snapshots
  have no good OCI home; Nydus's read-only-base assumption
  is incompatible with Engram's mutable-runtime-state model.
- **Hybrid by artifact type** (chunks-in-OCI for distributed
  images, BlobStorage-keyed for internal bakes). Long-term
  shape if external/customer-supplied images become common:
  `image_cache` fans out on artifact type at pull time.
  Captured in Rollout §5; not the v1 path.
- **SOCI ztoc instead of Nydus chunk blob.** Considered.
  SOCI preserves Trivy compatibility natively (image stays a
  conventional tar.gz). Rejected because tar-offset random
  access doesn't deliver the byte-aligned chunk-COW that
  NBD / UFFD / canonical-memory MAP_PRIVATE all require.
  Could become a third path for the conventional-image case
  under the hybrid alternative above.
- **File-level chunking (Nydus-default).** Stronger cross-
  image dedup but requires replacing NBD with virtiofs/RAFS,
  changes VZ's substrate story, and is an ADR-sized follow-on
  commitment. Deferred to a follow-on; migration shape and
  library landscape sketched in §Future direction.

## Rollout

Tracked in `docs/chunked-storage-rollout.md` under "ADR 0008
migration." Five sub-phases:

1. **`ChunkResolver` trait introduced.** Default impl wraps
   current `BlobStorage`. No behavior change. Clean
   abstraction point for tiered fetch.
2. **`OciChunkResolver` + Range GET in `engram-oci`.** Fault
   path falls through to OCI for chunks not in BlobStorage.
   Image-builder unchanged; enables the lazy-fill path with
   no bake changes. **This phase alone fixes the cross-
   namespace bricked-image failure mode** even before the
   Nydus-shaped artifact lands.
3. **Bake produces Nydus-shaped artifacts.** New media types,
   new bootstrap+blob layers. Image-builder gains scan + SBOM
   + referrer-publish step. `ImageBundle` schema bumps to v2.
4. **Base/diff layer engineering.** Cross-image dedup
   preserved across the bake pipeline. Image-builder consults
   parent bootstrap at build time to skip chunks already in
   the base layer.
5. **Hybrid image_cache.** Conventional OCI and chunked-OCI
   artifacts coexist; format detected at pull time; both
   paths run.

Per-phase gates and exit criteria tracked in the rollout doc.

## What this ADR does NOT cover

- **Choice of scanner.** Trivy is the assumed first
  integration; the bake step is scanner-agnostic. Per-org
  scanner selection is a deployment-time choice.
- **Referrer-artifact lifecycle and GC.** Registries vary in
  whether deleting an image evicts its referrers. Per-
  registry follow-up.
- **OCI registry sizing under chunk-blob fault load.** High-
  frequency Range GET traffic patterns vary widely across
  registry implementations. Validation gated in Rollout §2.
- **Snapshot promotion to OCI.** Long-lived snapshots
  archived as chunked OCI artifacts for cross-fleet
  portability ("publish session state as a new image"
  workflows). Useful but deferred.
- **User-facing warmup mechanism.** The `[bake.warmup]` knob
  in `engram.toml` that turns ADR 0007's canonical-memory
  primitive into a user feature. Designed in a follow-up ADR;
  this ADR's contribution is the distribution path for the
  resulting memory snapshots.
- **File-level chunking.** Covered as future direction below;
  not pursued as part of this ADR's rollout.
- **AWS S3 / non-GCS BlobStorage backends.** Same deferred
  decision as ADR 0007.

## Future direction: file-aligned chunking

The disk-aligned chunking choice in §Chunk granularity
preserves compatibility with NBD-from-chunks on FC and
materialize-to-file on VZ. Its known limitation is **weaker
cross-image dedup for unrelated images that share file content
but have different ext4 layouts**. Sibling tags and
snapshot-vs-base dedup fine because they share inode
placement; two independently-bakes images that both happen to
contain `/usr/lib/python3.11/...` may not collide at the 16
MiB chunk level even though the underlying file bytes are
identical.

Should that gap become operationally significant — surfaced
via storage-growth telemetry on the BlobStorage namespace —
the migration shape is sketched here so a future ADR can pick
it up without re-discovering the design space.

### Substrate change

- **Guest sees a filesystem, not a block device.** The ext4
  image goes away; the guest mounts the chunk-backed
  filesystem directly.
- **Bake** emits a RAFS bootstrap (file-tree metadata + chunk
  hashes) and per-file chunks, replacing the ext4 packing
  step.
- **FC runtime** has two viable paths:
  - *Userspace daemon* — run `nydusd` (or `virtiofsd` backed by
    a RAFS FUSE mount), expose virtio-fs to FC via
    vhost-user-fs. FC has supported virtio-fs since v1.0.
  - *EROFS + fscache on-demand* (kernel 5.19+) — RAFS v6 is
    bytecompatible with the in-kernel EROFS filesystem. The
    kernel mounts the image directly and fscache serves
    chunk-faults from the chunk store. **No userspace daemon
    in the hot path.** Cleanest runtime model on Linux.
- **VZ runtime** is the asymmetric part. VZ's
  `VZVirtioFileSystemDeviceConfiguration` shares a host
  directory, not a FUSE backend. Three options, none clean:
  - Materialize the file tree to a real directory at session
    create. Loses lazy-fault on macOS; acceptable if VZ stays
    dev-only.
  - Back the shared directory with `fuse-t` (userspace FUSE
    on macOS via NFS-loopback). Adds latency and a kernel
    extension cloud; needs benchmarking before committing.
  - Accept VZ on materialize-to-file while FC gets the lazy
    path. Asymmetric but tolerable for a dev backend.

### Memory side is unchanged

Memory chunks remain byte-aligned at 512 KB and chunked from
`memory.bin`. Canonical-memory MAP_PRIVATE, UFFD-from-chunks,
and working-set traces don't depend on the disk substrate.

### Open-source landscape

The Rust ecosystem already covers most of this work:

- **`dragonflyoss/nydus`** (Apache-2.0, CNCF Sandbox) —
  production-mature RAFS implementation in Rust. The
  `nydus-storage`, `rafs`, `nydus-builder`, and `nydusd`
  crates cover bake-side conversion, the bootstrap format,
  the chunk-store abstraction, and the runtime daemon.
  ~60-70% of the implementation lives here.
- **`virtio-fs/virtiofsd`** (Apache-2.0, Rust) — reference
  virtio-fs daemon if a thinner waist than `nydusd` is wanted.
- **Linux EROFS + fscache** (kernel; GPL) — the no-userspace-
  daemon path; requires `CONFIG_EROFS_FS_ONDEMAND=y` and a
  small userspace registration tool.
- **`cberner/fuser`** (MIT) — lower-level FUSE if custom
  handlers are needed; probably not necessary if Nydus is the
  source.
- **`macos-fuse-t/fuse-t`** (BSD-style) — speculative path
  for VZ integration on macOS without a kernel extension.
- **OverlayFS** (kernel built-in) — the natural
  writable-overlay layer for snapshot capture in the file-
  aligned world; replaces today's chunked-disk dirty buffer.

### Effort estimate

~10-14 weeks for one engineer:

- Bake-side conversion to RAFS (~2-3 weeks)
- FC virtio-fs daemon integration or EROFS+fscache wiring
  (~4-5 weeks)
- Snapshot capture via OverlayFS upper-dir chunking (~2 weeks)
- VZ path decision + implementation (~2-3 weeks, range
  depending on whether materialize-to-file is acceptable)

Vs. ADR 0008's ~6-9 weeks for the disk-aligned shape. The
delta is mostly the FC substrate swap (NBD → virtio-fs/EROFS)
and the VZ asymmetry handling.

### The signal to do it

This is a follow-on ADR, not a v1 commitment. The trigger is
operational data: **storage cost growth from unrelated images
failing to dedup at the durable layer**, traceable via
chunk-store inspection of cross-image hash collision rates. If
the BlobStorage namespace shows many images consuming GiB of
storage without overlap that file-aligned chunking would have
collapsed, that's the signal.

The clearest first beneficiary is a multi-tenant deployment
with diverse base images. Engram's current closed-fleet image
set (internal bakes off a small number of base images) doesn't
exercise the gap yet.
