# Chunked-Immutable Storage + Cloud-Agnostic Deployment

## Context

Two intertwined goals that should ship together because they're each meaningless without the other:

1. **Production-grade storage substrate**: replace the current `tar+zstd → BlobStorage` cold-tier pipeline with a chunked-immutable-disk-and-memory model. The current path solves "store the state in GCS" but loses the operationally-important properties — fast cross-host migration, low-storage-cost dedup, sub-100ms restore, COW at three levels (disk, memory, session fork). Without these, spot/preemptible hosts aren't viable, rolling deploys trash user state, and per-session cost stays high.

2. **Cloud-agnostic deployment artifacts**: Helm chart for the coordinator, Packer images and Terraform modules for FC hosts, an explicit infrastructure contract that other clouds can satisfy. Today Engram has a working dev stack but zero production deployment artifacts (no `.tf`, no Helm chart, no Packer manifest). The deploy story for the user (Cortex on GCP) needs to be both *real and reusable*.

User-stated constraints:
- **No backwards compatibility.** Delete the tar.zst flush pipeline, the seal-blob-ref machinery, the cold-tier columns on `snapshots`, the disk-pressure detector. Nothing is deployed yet.
- **Dev as similar to prod as possible.** Same code paths in `just dev` and in `--mode=coordinator` on GKE; the only difference is which `BlobStorage` impl chunks land in and whether `NBD` (Linux+FC) vs materialize-to-file (macOS+VZ) is used.
- **Cloud-agnostic from day one.** GCP first-class; AWS deferred but the contract is documented so a future PR can drop in.
- **Production-ready, not v1-easy.** Memory snapshot strategy uses the production techniques (MAP_PRIVATE of canonical base + working-set record-and-replay + 512 KB chunks for memory, 16 MiB chunks for disk), not the simpler monolithic-blob approach.

ADR 0007 captures the architecture; this plan lands the implementation.

---

## Decisions baked in

| Question | Decision | Why |
|---|---|---|
| Disk chunk size | **16 MiB** | Replit's published size; sequential-access-friendly; one GCS PUT per chunk is right scale |
| Memory chunk size | **512 KB** | AWS Lambda's published size; small enough to avoid wasted upload on partial dirty, large enough to keep manifest manageable |
| Memory chunk addressing | **Content-hash (sha256)** | Enables canonical-base sharing; deduplicates identical chunks across snapshots automatically |
| Memory dedup across VMs | **MAP_PRIVATE of canonical base snapshot** | Hardware-enforced COW via MMU; no userspace page-hashing (avoids KSM-style side-channel concerns); same technique Lambda uses |
| Working-set capture | **Record-and-replay** | After first UFFD restore on a host, record chunk-access trace; prefault that trace synchronously on subsequent restores before vCPU runs. ~3.7× speedup per REAP |
| Cache tiers (v1) | **Two-tier (local NVMe + GCS)** | L2 regional shared cache deferred; LRU on local NVMe with budget |
| Object storage abstraction | **Existing `BlobStorage` trait** | Already supports GCS, S3, MinIO, local fs; chunk store is a layer above it |
| AWS Terraform | **Deferred / contract-documented** | First-class is weeks of work for zero current users; contract doc keeps the door open |
| Memory snapshot path on macOS | **No memory snapshot for VZ in v1** | VZ's native memory snapshot is broken upstream for arm64 guests (ADR 0003); APFS-clone-the-disk gives session-restart parity. Memory COW is FC-only |
| Cold-tier flush pipeline | **Deleted entirely** | Replaced by chunk store + LRU GC. `engram-host-agent::flush`, the disk-pressure detector, blob seal/unseal helpers, cold-tier `SnapshotRecord` columns all retire |
| Coordinator deploy | **Helm chart, cloud-agnostic templates** | k8s itself is the abstraction; cloud-specifics live in `values.yaml` |
| FC-host deploy | **Per-cloud Packer + Terraform** | No useful cloud-agnostic abstraction over MIG vs ASG; share the *contract* and the *drain hook* |
| Cross-component upgrade order | **Coordinator before host-agent for additive changes; reverse for forward-compat** | N-1 wire-skew policy like kubernetes |

---

## Architecture overview

Three layers, each replacing something concrete in today's code:

```
┌────────────────────────────────────────────────────────────────┐
│  Layer 3: SandboxBackend integration                           │
│  - FirecrackerBackend uses NBD for disk + UFFD-from-chunks     │
│    for memory                                                  │
│  - VzBackend uses materialize-to-file (disk only; memory       │
│    snapshot deferred)                                          │
│  - ProcessBackend uses chunked-rootfs (cwd-mounted)            │
│                                                                │
│  SandboxBackend trait:                                         │
│    snapshot(id) -> ManifestRef         (was: dest: &Path)      │
│    restore(manifest_ref) -> SandboxId   (was: src: PathBuf)    │
│    SandboxSpec.rootfs: ManifestRef      (was: PathBuf)         │
└─────────────────────────┬──────────────────────────────────────┘
                          │
                          ▼  consumes:
┌────────────────────────────────────────────────────────────────┐
│  Layer 2: Block/page-device adapters (platform-specific)       │
│                                                                │
│  - Linux + FC + chunked disk:                                  │
│    NBD daemon → kernel /dev/nbd0 → FC virtio-blk               │
│  - Linux + FC + chunked memory:                                │
│    UFFD handler → 4 KiB faults → 512 KB chunk fetches →        │
│    MAP_PRIVATE canonical + per-session delta                   │
│  - macOS + VZ + chunked disk:                                  │
│    Materialize manifest to file, attach as virtio-blk,         │
│    APFS clonefile() for COW on snapshot                        │
│  - ProcessBackend + chunked rootfs:                            │
│    Chunks streamed to a cwd at session start                   │
└─────────────────────────┬──────────────────────────────────────┘
                          │
                          ▼  consumes:
┌────────────────────────────────────────────────────────────────┐
│  Layer 1: Chunk store (cloud-agnostic, identical everywhere)   │
│                                                                │
│  - Manifests (versioned, content-addressed)                    │
│  - Chunks (content-hash-addressed, immutable)                  │
│  - Storage via existing BlobStorage trait                      │
│  - Local NVMe cache with LRU + budget                          │
│  - GC by manifest reachability                                 │
│  - Working-set traces (per-manifest, host-local recordings)    │
└────────────────────────────────────────────────────────────────┘
```

The bottom layer is the same Rust code on Linux, macOS, dev, production — only what's *underneath* `BlobStorage` differs (local FS for dev, GCS for production, S3 for AWS).

---

## Copy-on-write — three levels you get for free

This is the production payoff, not a separate feature:

**1. Disk COW** — base image manifest's chunks are shared by N sessions. Each session has its own manifest. Writes produce new chunks; the session's manifest gets new pointers for dirty offsets. The base manifest never changes. Cross-session disk dedup is automatic via content addressing.

**2. Memory COW** — image-builder boots the VM once and captures `memory.bin` at steady state (post-init, pre-work). That's the **canonical memory snapshot**. New sessions `mmap(addr, len, PROT_READ|PROT_WRITE, MAP_PRIVATE, fd, 0)` against the canonical file. Initially zero physical pages allocated per session — reads come from host page cache (one copy for the canonical, shared by all sessions). On first write to any page, MMU page-fault handler allocates a private copy. Hardware-enforced, no userspace page hashing required.

Practical impact: 1000 Python sessions each with 4 GiB RAM reservations consume 4 GiB (canonical) + 1000 × ~100 MiB (per-session deltas) ≈ 100 GiB total, not 4 TiB.

**3. Session-fork COW** — `POST /sessions/:id/fork` is two small writes (a Postgres row + a manifest JSON in GCS). Forked + parent diverge only as either writes; both reference the same chunks. Useful for "try N approaches from this checkpoint" and "time-travel debugging." Falls out of content-addressing for free.

ADR 0007's "Consequences" section documents these three as the load-bearing reasons.

---

## Component design

### Chunk store crate (`engram-chunk-store`)

New crate. Pure storage logic; no VM integration; runs identically on Linux/macOS/dev/prod.

**Types**:

```rust
// Manifest is a versioned, immutable view of a "virtual disk"
// or "virtual memory image." Stored as one object per version
// in BlobStorage.
pub struct Manifest {
    pub schema_version: u32,
    pub kind: ManifestKind,  // Disk { chunk_size: 16 MiB } | Memory { chunk_size: 512 KB }
    pub total_bytes: u64,
    pub chunks: Vec<ChunkRef>,
    pub parent: Option<ManifestRef>,  // For fork: shallow copy of parent's chunk list
    pub working_set_trace: Option<TraceRef>,  // For memory: prefault hint
}

pub struct ChunkRef {
    pub offset: u64,
    pub hash: ChunkHash,  // sha256
}

pub struct ManifestRef {
    pub manifest_id: Uuid,    // Stable identifier across versions
    pub version: u64,         // Monotonic per manifest_id
}

pub struct ChunkHash([u8; 32]);

// Working-set trace: chunks faulted in during the first N seconds
// of a restore. Captured per-(manifest, host) on first restore;
// replayed on subsequent restores.
pub struct WorkingSetTrace {
    pub chunks: Vec<ChunkHash>,  // In access order
    pub captured_at: DateTime<Utc>,
    pub vcpu_count: u32,
}
```

**Operations**:

```rust
impl ChunkStore {
    pub async fn put_manifest(&self, m: Manifest) -> Result<ManifestRef>;
    pub async fn get_manifest(&self, r: ManifestRef) -> Result<Manifest>;
    pub async fn fork_manifest(&self, src: ManifestRef) -> Result<ManifestRef>;

    pub async fn put_chunk(&self, body: &[u8]) -> Result<ChunkHash>;
    pub async fn get_chunk(&self, h: ChunkHash) -> Result<Bytes>;

    pub async fn record_trace(&self, m: ManifestRef, t: WorkingSetTrace) -> Result<()>;
    pub async fn get_trace(&self, m: ManifestRef) -> Result<Option<WorkingSetTrace>>;

    pub async fn gc_unreferenced(&self, retain_for: Duration) -> Result<usize>;
}
```

**Storage layout in BlobStorage** (so backend impls don't need to know chunk-specific details):

```
chunks/sha256/<first-2-hex>/<rest-of-hash>   (immutable, content-addressed)
manifests/<manifest_id>/v<version>.json       (immutable, versioned)
traces/<manifest_id>/<host_id>.json           (host-local, periodically updated)
```

**Local NVMe cache** (`engram-chunk-store::cache`):
- LRU-bounded by configurable budget (default 200 GiB per host)
- `cache.get_or_fetch(hash)` — local hit returns immediately; miss triggers `BlobStorage::get` + cache fill
- `cache.put(hash, bytes)` — writes to NVMe; evicts oldest if over budget
- Pin chunks in the working-set trace (don't evict them between restores)

**Files**: new crate `crates/engram-chunk-store/`. Depends on `engram-core` (for BlobStorage trait) and `sha2`, `serde`, `serde_json`, `tokio`. ~2000 lines including tests.

### Disk adapter — Linux NBD daemon

New module in `engram-host-agent` (or its own crate `engram-chunk-disk` if it gets big). Serves chunked manifests as NBD block devices that FC can attach.

**Architecture**:
- Listens on a Unix socket per session (`/var/run/engram/disk-<sandbox_id>.sock`)
- Speaks the NBD protocol to the kernel's `nbd-client` (or directly to `/dev/nbd*`)
- Reads resolve to `cache.get_or_fetch(chunks[offset/chunk_size].hash)`, served from local NVMe
- Writes coalesce into in-memory dirty regions per 16 MiB chunk; periodically flushed
- `flush()` operation: hash dirty regions, PUT new chunks, update manifest version, commit

**FC integration**:
- `FirecrackerBackend::create` no longer takes `spec.rootfs_source: PathBuf`
- Instead, takes `spec.rootfs: ManifestRef`
- Spawns the NBD daemon for that manifest, kernel attaches `/dev/nbd0`, FC's `path_on_host` points at `/dev/nbd0`

**Files**:
- `crates/engram-host-agent/src/disk_daemon.rs` (new, ~800 lines)
- `crates/engram-sandbox-firecracker/src/lib.rs` (refactor `create`, `restore`, `destroy`)

### Disk adapter — macOS materialize-to-file

For VZ on macOS dev. No NBD; the chunk daemon writes the assembled disk to a regular file before VM start.

**Architecture**:
- `materialize(manifest) -> PathBuf` — concatenates chunks into a file at `<work_dir>/<sandbox_id>/disk.img`
- VZ attaches this file as virtio-blk via `DiskImageStorageDeviceAttachment` (existing path)
- APFS `clonefile(2)` for per-sandbox COW (already what VZ uses today; just operating on the materialized file)
- Snapshot: hash-and-compare the disk against the manifest at pause time, rechunk dirty regions
  - Slow (O(disk size)) but correct and simple
  - Optimization later: maintain a write log via FUSE / a custom VZ disk attachment

**Files**:
- `crates/engram-sandbox-vz/src/disk.rs` (refactor: drop file-path-as-rootfs, use ManifestRef)
- `crates/engram-sandbox-vz/src/backend.rs` (materialize before VM start, hash-and-compare on snapshot)

### Memory adapter — UFFD-from-chunks (Linux + FC, the production magic)

The biggest piece. Replaces `engram-uffd-handler`'s current "read from memory.bin file" logic with "read from chunked memory store + canonical-base MAP_PRIVATE + working-set prefault."

**Architecture**:

```
1. FC asks for memory file at restore time:
   FC: "give me a fd backing the guest's memory address space"
   
2. Engram's UFFD handler responds:
   - mmap canonical_memory.bin (canonical_base_path) with MAP_PRIVATE
   - This is the FD given to FC
   - host page cache holds canonical once; private pages allocated lazily
   
3. FC restores VM state, vCPUs start running:
   - Reads of "clean" pages: kernel serves directly from the page cache
   - Reads of pages NOT in the canonical (i.e., overridden by the session's delta):
     UFFD page-fault → handler resolves to the session manifest's chunk →
     fetches chunk from local NVMe or GCS → UFFDIO_COPY into guest memory
   - Writes: kernel allocates a private page, copies canonical, applies write
   
4. Working-set replay:
   - On restore, BEFORE letting vCPUs run, prefault all chunks listed in the
     session's working-set trace into the local cache and into the guest's
     memory map via UFFDIO_COPY
   - First-N-seconds page faults thus don't actually fault — pages are
     already there
   - Result: sub-100ms wall-clock to "VM is doing useful work"
   
5. Recording (first restore on a host):
   - Track every chunk faulted in during the first 5s after vCPU start
   - On snapshot or session end, persist trace as
     traces/<manifest_id>/<host_id>.json
   - Subsequent restores read the trace and use it for step 4
```

**Why this gets sub-100ms (vs today's ~750ms typical UFFD restore)**:
- The canonical pages are already in the host page cache (any recent Python session warmed it)
- The session-specific delta is small (~50-200 MiB typically)
- The working-set trace prefaults the chunks the VM will actually need first
- Cross-vCPU fault storms don't serialize on the handler because the working set is loaded synchronously before vCPUs unfreeze

**Files**:
- `crates/engram-uffd-handler/src/main.rs` (substantial rewrite, ~1500 lines)
- `crates/engram-uffd-handler/src/canonical.rs` (new: mmap canonical, MAP_PRIVATE setup)
- `crates/engram-uffd-handler/src/working_set.rs` (new: record + replay)
- `crates/engram-uffd-handler/src/chunk_resolver.rs` (new: page address → chunk hash → bytes)

### Image-builder rewrite (canonical base snapshots)

The image-builder gains a new responsibility: produce the canonical memory snapshot during bake.

**New bake pipeline**:

1. **Docker build → rootfs export** (existing)
2. **Inject agentd + bootstrap + harness binaries** (existing)
3. **Chunk the rootfs**: split ext4 into 16 MiB chunks, hash each, write to chunk store. Produce disk manifest.
4. **Boot the image once with Firecracker** (NEW):
   - Start FC with the chunked disk attached via NBD daemon
   - Let the bake's init script run; wait for "ready" signal (typically a sentinel file or a port listen)
   - FC: PATCH /vm Paused
   - Capture memory.bin (4-16 GiB)
   - Chunk memory.bin into 512 KB chunks, write to chunk store. Produce memory manifest.
5. **Capture initial working-set trace**:
   - Restore the snapshot, run the same "ready" script for 5s, record chunks accessed
   - Persist as the canonical working-set trace for this image
6. **Push to OCI registry**:
   - The image_uri now points at a small manifest object listing disk-manifest-ref, memory-manifest-ref, and working-set-trace-ref (rather than at the ext4 file directly)

**Files**:
- `crates/engram-image-builder/src/lib.rs` (major rewrite)
- `crates/engram-image-builder/src/canonical_boot.rs` (new: boot once during bake)
- `crates/engram-image-builder/src/chunker.rs` (new: file → 16 MiB chunks or 512 KB chunks)

### SandboxBackend trait refactor

```rust
// New trait shape:
pub trait SandboxBackend: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotRef, SandboxError>;
    // Returns a SnapshotRef (manifest IDs for disk + memory), not a Path.
    
    async fn restore(&self, snap: SnapshotRef) -> Result<SandboxId, SandboxError>;
    // Restores from a SnapshotRef. Backend resolves manifests internally.
    
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    // ... existing methods (exec_stream, start_agent, notify_session_policy, etc.) unchanged
}

pub struct SandboxSpec {
    pub image: String,             // human-readable tag
    pub rootfs: ManifestRef,       // was: Option<PathBuf>
    pub memory_canonical: Option<ManifestRef>,  // new: for FC fast-restore
    pub working_set_trace: Option<TraceRef>,    // new: for FC fast-restore
    // ... other fields unchanged
}

pub struct SnapshotRef {
    pub disk: ManifestRef,
    pub memory: Option<ManifestRef>,  // None for VZ in v1
}
```

### MetadataStore / Postgres schema changes

**Migration `0018_chunked_storage.sql`** (drops cold-tier columns, adds manifest references):

```sql
ALTER TABLE snapshots
    -- Drop the cold-tier sealed-blob-ref quartet
    DROP COLUMN wrapped_dek,
    DROP COLUMN nonce,
    DROP COLUMN ciphertext,
    DROP COLUMN key_id,
    DROP COLUMN blob_present,
    DROP COLUMN replicated_at,
    -- Drop local_path (chunks live in BlobStorage, no local-only state)
    DROP COLUMN local_path,
    -- Add manifest references
    ADD COLUMN disk_manifest_id UUID NOT NULL,
    ADD COLUMN disk_manifest_version BIGINT NOT NULL,
    ADD COLUMN memory_manifest_id UUID,
    ADD COLUMN memory_manifest_version BIGINT,
    -- Working set trace ref (host_id only — the trace is host-local)
    ADD COLUMN working_set_trace_host UUID;

-- The new resilience index: every snapshot has a disk manifest; cross-host
-- restore is possible for any snapshot with a memory manifest.
CREATE INDEX idx_snapshots_disk_manifest ON snapshots (disk_manifest_id);
DROP INDEX IF EXISTS idx_snapshots_residency;
```

`SnapshotRecord` struct:

```rust
pub struct SnapshotRecord {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub host_id: HostId,  // The host that took the snapshot; affinity hint, not requirement
    pub image_version: String,
    pub disk_manifest: ManifestRef,
    pub memory_manifest: Option<ManifestRef>,
    pub working_set_trace_host: Option<HostId>,
    pub size_bytes: u64,  // From chunk-store accounting
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
}

// `SnapshotResidency` enum deleted — there's only one tier now.
```

`MetadataStore` methods:
- `record_snapshot(SnapshotRecord)` (kept; new fields)
- `latest_snapshot_for_session(SessionId)` (kept; returns the new shape)
- `flush_to_cold`, `clear_local_path`, `latest_cold_snapshot_for_session` — **deleted**.
- `list_idle_sessions` — kept for the eviction path.

### Coordinator API simplification

**`api/snapshot.rs`**:

The three-branch dispatcher in `ensure_active` simplifies dramatically. Today: `Idle` → hot resume, `ColdEvicted` → cold resume, `Dead` → 410. After: `Idle` → resume (single path), `Dead` → 410. No tier dichotomy.

- `POST /sessions/:id/snapshot` — same shape; backend returns `SnapshotRef`
- `POST /sessions/:id/resume` — single resume path; no host affinity required for cold (every host can serve any manifest with sub-second startup)
- `DELETE /sessions/:id/local` — replaced by `evict_session`. Drops the VM but keeps the snapshot manifest. Session goes `Idle`.

**`api/admin.rs`**:

- `POST /api/admin/sessions/:id/flush` — **deleted**. There's no separate flush; snapshots are already in BlobStorage via chunks.
- `POST /api/admin/flush-idle` — **deleted** same reason.
- Replaced by `POST /api/admin/sessions/:id/evict` — drop the VM, keep the manifest. The only useful admin primitive now.

`engram-host-agent/src/disk_pressure.rs` — **deleted**. Replaced by chunk-store LRU GC on the local cache.

`engram-host-agent/src/flush.rs` — **deleted**.

`engram-coordinator/src/blob.rs` (seal/unseal) — **deleted** for snapshot blob URLs. (The `engram-crypto::CredCipher` machinery itself stays because `registry_credentials` and `session_secrets` still use it.)

---

## Deployment artifacts

### Helm chart for coordinator

```
deploy/helm/engram-coordinator/
├── Chart.yaml
├── values.yaml                 # cloud-agnostic defaults
├── values-gcp.yaml.example     # GCP-specific (Workload Identity, internal LB annotations)
├── values-aws.yaml.example     # AWS-specific (IRSA, NLB annotations)
├── templates/
│   ├── deployment.yaml
│   ├── service.yaml
│   ├── serviceaccount.yaml     # cloud-specific annotations come from values
│   ├── configmap.yaml
│   ├── secretproviderclass.yaml  # rendered conditionally based on values.secrets.driver
│   ├── hpa.yaml
│   ├── pdb.yaml
│   ├── networkpolicy.yaml
│   └── ingress.yaml
└── README.md
```

Cloud-specific bits live entirely in `values.yaml` (annotations, secret-provider config, LB type). Templates are pure k8s.

Coordinator deploys via Helm = K8s rolling restart. Standard. ~2s SSE blip; active sessions on FC hosts unaffected (their state is in chunk store, not on coordinator).

### Packer for FC host image

```
deploy/packer/
├── fc-host-gcp.pkr.hcl        # Builds a GCE custom image
├── fc-host-aws.pkr.hcl        # Builds an AMI (deferred; doc only)
└── provisioners/              # Shared scripts; cloud-agnostic
    ├── install-firecracker.sh
    ├── install-host-agent.sh
    ├── install-chunk-daemon.sh
    ├── install-uffd-handler.sh
    ├── systemd/
    │   ├── engram-host-agent.service
    │   ├── engram-chunk-daemon.service
    │   └── engram-uffd-handler.service  # spawned per-restore by host-agent, not unit
    └── engram-drain.sh         # Universal drain hook
```

`engram-drain.sh` is the universal piece — calls `POST /api/hosts/$HOST_ID/drain?migrate_active=true&deadline_secs=$DEADLINE` regardless of cloud. Wired into the Packer image's systemd unit's `ExecStop=`.

### Terraform module: GCP reference

```
deploy/terraform/gcp/
├── main.tf
├── variables.tf
├── outputs.tf
├── modules/
│   ├── network/                # VPC, subnet, NAT, firewall
│   ├── gke/                    # GKE cluster + node pool for coordinator
│   ├── postgres/               # Cloud SQL with HA
│   ├── secrets/                # Secret Manager entries + IAM
│   ├── storage/                # GCS bucket for chunks (lifecycle rules)
│   ├── artifact-registry/      # OCI repo for engram images
│   ├── fc-host-mig/            # MIG for FC hosts, MIG autoscaler, drain wiring
│   └── workload-identity/      # k8s SA ↔ GCP SA bindings
└── examples/
    └── minimal/                # Smallest viable deploy
```

The GCP module's outputs (LB IP, DB connection name, bucket name, etc.) feed directly into Helm values. Tightly coupled at the interface, independent at implementation.

### The infrastructure contract

`deploy/README.md` documents what any cloud must provide:

1. Postgres ≥14 (with `LISTEN/NOTIFY`)
2. Object storage satisfying `BlobStorage` (chunks live here)
3. Secret store satisfying `SecretStore` + `CaSource`
4. Linux + KVM-capable VMs for FC hosts (any provisioning mechanism)
5. K8s for coordinator (optional; coordinator runs fine as a single-process VM too)
6. Network connectivity (FC hosts → coordinator, coordinator → DB / storage / secrets)

GCP module is one realization; the contract lets others (AWS, bare metal, on-prem) drop in.

---

## Implementation phases

Order chosen so each phase is independently shippable and lower phases never depend on higher ones. Macos-first for the storage adapters so dev catches bugs before production.

### Phase 1 — Chunk store foundations

**Goal**: `engram-chunk-store` crate, no VM integration, no production callers yet. Pure storage logic.

**Deliverables**:
- New crate with `Manifest`, `ChunkRef`, `ChunkHash`, `WorkingSetTrace`, `ChunkStore` API
- `ChunkStore` impl that backs onto `BlobStorage` trait
- Local NVMe LRU cache with budget
- GC by manifest reachability + retention TTL
- Unit tests against in-memory + local-filesystem `BlobStorage`

**Files created**:
- `crates/engram-chunk-store/Cargo.toml`
- `crates/engram-chunk-store/src/lib.rs`
- `crates/engram-chunk-store/src/manifest.rs`
- `crates/engram-chunk-store/src/store.rs`
- `crates/engram-chunk-store/src/cache.rs`
- `crates/engram-chunk-store/src/gc.rs`
- `crates/engram-chunk-store/src/working_set.rs`

**Files modified**:
- `Cargo.toml` (workspace) — add the new crate
- `crates/engram-storage-gcs/src/lib.rs` — already supports the operations we need

### Phase 2 — Image-builder produces chunked artifacts

**Goal**: Image-builder writes a disk manifest (no canonical memory yet). VZ + Process backends can attach this.

**Deliverables**:
- Image-builder rewritten to:
  - Build the ext4 (or directory) as before
  - Chunk it into 16 MiB chunks, write to chunk store, produce disk manifest
  - Push manifest + chunks to remote BlobStorage (or leave locally for dev)
- The image-builder OCI push now publishes a small "image bundle" object listing `disk_manifest_ref` and (initially) no memory artifacts
- Backward incompatibility: existing OCI-pulled images become invalid. **No migration; rebuild all images.**

**Files modified**:
- `crates/engram-image-builder/src/lib.rs` (major)
- `crates/engram-image-builder/src/chunker.rs` (new)
- `crates/engram-oci/` — manifest format for the image bundle changes
- `crates/engram-host-agent/src/image_cache.rs` — `CachedImage` now returns `ManifestRef`, not `PathBuf`

### Phase 3 — Disk adapter for macOS (VZ + Process)

**Goal**: macOS dev workflow works end-to-end against chunked disks.

**Deliverables**:
- VZ backend materializes manifest → disk file before VM start
- VZ snapshot: hash-and-compare against current manifest, rechunk dirty, write new manifest version
- ProcessBackend uses chunk-store-backed rootfs (chunks become a cwd at session start)
- `just dev` works zero-config (uses `LocalBlob` for chunks)

**Files modified**:
- `crates/engram-sandbox-vz/src/disk.rs` (substantial)
- `crates/engram-sandbox-vz/src/backend.rs`
- `crates/engram-sandbox-process/src/lib.rs`
- `crates/engram-core/src/types/sandbox.rs` (`rootfs_source: Option<PathBuf>` → `rootfs: ManifestRef`)

### Phase 4 — Disk adapter for Linux + FC

**Goal**: Linux + FC backend uses NBD-served chunked disks.

**Deliverables**:
- NBD daemon (`engram-host-agent/src/disk_daemon.rs`) speaking NBD protocol to kernel `/dev/nbd*`
- `FirecrackerBackend::create` spawns the NBD daemon, attaches the kernel block device
- FC `path_on_host` points at `/dev/nbd<N>`
- Per-session dirty region tracking, periodic flush, manifest version update
- Integration tests against the Linux dev VM

**Files modified**:
- `crates/engram-host-agent/src/disk_daemon.rs` (new, ~800 lines)
- `crates/engram-sandbox-firecracker/src/lib.rs` (major: `create`, `destroy`)
- `crates/engram-host-agent/src/lib.rs` — wire up daemon spawn

### Phase 5 — Memory adapter for Linux + FC (the production magic)

**Goal**: Sub-100ms restore for FC sessions via canonical-memory MAP_PRIVATE + working-set replay.

**Deliverables**:
- Image-builder boots the VM during bake, captures canonical memory snapshot, chunks it (512 KB), records canonical working-set trace
- `engram-uffd-handler` rewrite:
  - Accepts a SnapshotRef + canonical-memory file path on startup
  - mmap canonical with MAP_PRIVATE → that's the FD given to FC
  - On UFFD page faults: resolve via session's memory manifest → chunk store → UFFDIO_COPY
  - Before vCPU run on restore: prefault working-set chunks synchronously
  - Records the actual chunk-access trace during the first 5s of vCPU runtime
  - Persists the trace as `traces/<manifest_id>/<host_id>.json` on snapshot
- `FirecrackerBackend::snapshot` + `restore` updated to use the new chunked memory path

**Files modified**:
- `crates/engram-image-builder/src/canonical_boot.rs` (new)
- `crates/engram-image-builder/src/lib.rs`
- `crates/engram-uffd-handler/src/main.rs` (major rewrite)
- `crates/engram-uffd-handler/src/canonical.rs` (new)
- `crates/engram-uffd-handler/src/working_set.rs` (new)
- `crates/engram-uffd-handler/src/chunk_resolver.rs` (new)
- `crates/engram-sandbox-firecracker/src/lib.rs` (snapshot / restore paths)

### Phase 6 — SandboxBackend trait + MetadataStore refactor

**Goal**: Trait + DB schema reflect the new world. Coordinator API simplifies.

**Deliverables**:
- `SandboxBackend::snapshot(id) -> SnapshotRef` (was `dest: &Path`)
- `SandboxBackend::restore(snap: SnapshotRef)` (was `src: PathBuf`)
- `SandboxSpec.rootfs: ManifestRef` + `memory_canonical: Option<ManifestRef>` + `working_set_trace: Option<TraceRef>`
- Migration `0018_chunked_storage.sql` drops cold-tier columns, adds manifest refs
- `SnapshotRecord` reshaped
- `MetadataStore::flush_to_cold`, `clear_local_path`, `latest_cold_snapshot_for_session` deleted
- `SnapshotResidency` enum deleted
- All three backends (Process, VZ, FC) updated to the new trait
- Coordinator API: `ensure_active` simplifies, three-branch becomes one
- `engram-protocol::wire::RequestKind::CreateSandbox` updated (spec change is a wire change → bump protocol version comment)

**Files modified**:
- `crates/engram-core/src/traits/sandbox.rs`
- `crates/engram-core/src/types/sandbox.rs`
- `crates/engram-core/src/types/snapshot.rs`
- `crates/engram-core/src/traits/metadata.rs`
- `crates/engram-postgres/src/lib.rs` (impl changes)
- `crates/engram-protocol/src/wire.rs` (`SandboxSpec` wire shape)
- `crates/engram-coordinator/src/api/snapshot.rs` (simplify)
- `crates/engram-coordinator/src/api/sessions.rs` (`ensure_active`)
- `deploy/migrations/0018_chunked_storage.sql` (new)

### Phase 7 — Delete v1 tar.zst path

**Goal**: Zero dead code. Everything that referenced the old cold-tier flush pipeline is gone.

**Deletions**:
- `crates/engram-host-agent/src/flush.rs`
- `crates/engram-host-agent/src/disk_pressure.rs`
- `crates/engram-coordinator/src/blob.rs` (the snapshot seal/unseal — `engram-crypto::CredCipher` itself stays)
- `crates/engram-coordinator/src/api/admin.rs::flush_one`, `flush_idle`
- The `engram-host-agent::disk_pressure::spawn` call from `engram-coordinator::lib.rs::start_coordinator`
- Any leftover references to `local_path` outside the cache layer

**Cleanups**:
- `Cargo.toml` deps for crates that referenced the deleted code
- `docs/deploy.md` — section on "cold-tier flush" rewritten or removed

### Phase 8 — Helm chart for coordinator

**Goal**: `helm install engram engram-coordinator` works on GKE, EKS, k3s, anywhere.

**Deliverables**:
- `deploy/helm/engram-coordinator/` complete
- Per-cloud example values files
- Tested against a `kind` cluster (cloud-agnostic) + a real GKE cluster (production-shape)

**Files created**: see "Deployment artifacts" section above.

### Phase 9 — Packer + GCP Terraform

**Goal**: `terraform apply` in `deploy/terraform/gcp/examples/minimal/` produces a working Engram deployment.

**Deliverables**:
- Packer manifest for GCE FC host image
- Packer-built systemd units for host-agent, chunk daemon (the UFFD handler is per-restore, not a unit)
- Drain hook wired as `ExecStop=`
- GCP Terraform module with all submodules
- A `minimal` example that deploys and works end-to-end
- Documentation of the contract for other clouds

**Files created**: see "Deployment artifacts" section above.

### Phase 10 — ADR 0007 + doc updates

**Goal**: Architectural decision is captured for future contributors. Existing docs reflect the new world.

**Deliverables**:
- `docs/adr/0007-chunked-immutable-storage.md` (the headline ADR)
- `docs/deploy.md` rewritten for chunked storage + the new operational shape
- `docs/known-issues.md` — close anything affected; add new entries for any deferred work (e.g., AWS module, L2 cache, page-level memory dedup)
- `DESIGN.md` updates:
  - Architecture diagrams reflect chunk store layer
  - Source-of-truth table updated (chunk store joins Postgres + image registry; tar.zst removed)
  - Component descriptions updated
- `README.md` — the "Two snapshot tiers" framing is gone; one tier (chunks) is the only one

---

## Files modified / created / deleted (summary)

### New crates
- `crates/engram-chunk-store/` — manifest + chunks + cache + GC + traces

### Major rewrites
- `crates/engram-image-builder/src/lib.rs` (canonical boot + chunked output)
- `crates/engram-host-agent/src/image_cache.rs` (chunk-store-backed)
- `crates/engram-host-agent/src/disk_daemon.rs` (new, NBD)
- `crates/engram-uffd-handler/src/main.rs` (chunked memory + WS R&R)
- `crates/engram-sandbox-firecracker/src/lib.rs` (NBD disk + UFFD-from-chunks)
- `crates/engram-sandbox-vz/src/disk.rs`, `backend.rs` (materialize-to-file)
- `crates/engram-sandbox-process/src/lib.rs` (chunk-backed rootfs)
- `crates/engram-core/src/types/sandbox.rs` (`rootfs: ManifestRef`)
- `crates/engram-core/src/traits/sandbox.rs` (snapshot/restore signatures)
- `crates/engram-core/src/types/snapshot.rs` (`SnapshotRecord` reshape)
- `crates/engram-core/src/traits/metadata.rs` (snapshot methods)
- `crates/engram-postgres/src/lib.rs` (impl changes)
- `crates/engram-coordinator/src/api/snapshot.rs` (simplify)
- `crates/engram-coordinator/src/api/sessions.rs` (`ensure_active`)
- `crates/engram-coordinator/src/api/admin.rs` (drop flush endpoints)
- `crates/engram-protocol/src/wire.rs` (`SandboxSpec` wire change)

### Deleted
- `crates/engram-host-agent/src/flush.rs`
- `crates/engram-host-agent/src/disk_pressure.rs`
- `crates/engram-coordinator/src/blob.rs` (the snapshot helpers; CredCipher stays in engram-crypto)
- All `local_path` references and `blob_present`/`SnapshotResidency` logic
- `MetadataStore::flush_to_cold`, `clear_local_path`, `latest_cold_snapshot_for_session`

### New deploy artifacts
- `deploy/helm/engram-coordinator/` (chart)
- `deploy/terraform/gcp/` (reference module)
- `deploy/packer/` (per-cloud manifests + shared scripts)
- `deploy/README.md` (infrastructure contract)
- `deploy/migrations/0018_chunked_storage.sql`

### New docs
- `docs/adr/0007-chunked-immutable-storage.md`
- Updated: `docs/deploy.md`, `docs/known-issues.md`, `DESIGN.md`, `README.md`

---

## ADR 0007 outline (for Phase 10)

**Title**: ADR 0007: Chunked-immutable storage with canonical-base memory snapshots

**Status**: accepted, 2026-05-NN. **Supersedes** the cold-tier pipeline of ADR 0005 (snapshots are no longer two-tier; chunked storage is the single durability primitive).

**Context** — why the change:
- ADR 0005's tar.zst cold-tier flush gets the job done but loses operationally important properties: cross-host migration is slow (30s+ per session), spot/preemption windows can't accommodate flush of multi-session hosts, storage cost is N× redundant across similar sessions, memory dedup across VMs is impossible.
- Production references (AWS Lambda, Replit, Fly.io, AWS Aurora DSQL) have converged on chunked-immutable storage with content-addressed dedup as the right primitive. The published benefits — sub-100ms restore, near-free fork, COW at multiple layers, cross-host portability — are all things Engram needs.

**Decision**:
- Sandboxes' disk and memory state both live in a **chunk store**: content-addressed, immutable, in `BlobStorage`. Disk chunks 16 MiB; memory chunks 512 KB. Manifests are versioned, immutable references.
- Image-builder produces a **canonical memory snapshot** per image during bake, by booting the image once and capturing post-init RAM. Per-session memory snapshots are deltas against this canonical.
- FC restores `mmap` the canonical memory file with `MAP_PRIVATE`; kernel/MMU enforce copy-on-write across sessions sharing the same image. No userspace page hashing.
- **Working-set traces** are recorded on first restore per (manifest, host) pair and replayed (prefaulted before vCPUs run) on subsequent restores. Sub-100ms restore once warmed.
- The tar+zstd cold-tier flush pipeline is **deleted**; the disk-pressure detector is **deleted**; the seal-blob-ref machinery for snapshots is **deleted**. `SnapshotRecord` reshapes around `disk_manifest_ref` + `memory_manifest_ref`.

**Consequences**:
- **Disk COW**: free via content-addressed chunks.
- **Memory COW**: hardware-enforced via MAP_PRIVATE of canonical base.
- **Session fork COW**: O(manifest size) — a few KB to fork a session, useful for "try N approaches in parallel."
- **Cross-host migration**: pause → flush dirty chunks → bind to target → restore (with WS prefault). ~1-2s per session. Enables MIG rolling deploys without trashing sessions.
- **Spot/preemption viable**: 30s window is plenty to flush dirty deltas.
- **Storage cost**: dedup across sessions is automatic. Base image's GBs are stored once, regardless of how many sessions reference it.
- **Single-tier durability**: no more hot/cold dichotomy. Sessions live ↔ chunks reachable in BlobStorage.
- **MacOS dev**: gets disk COW via APFS clonefile + chunked-disk; memory COW deferred (VZ memory snapshot is broken upstream for arm64 anyway).
- **Backward incompatibility**: existing baked images become invalid. Rebuild all images.

**Alternatives considered (briefly)**:
- *Keep tar.zst, add chunked-disk only*: leaves the memory snapshot story (the bigger win) unsolved.
- *Page-level memory chunks (4 KiB)*: manifest explosion (~1M entries per 4 GiB VM). Production systems use 256 KB-1 MiB.
- *KSM-style cross-tenant page hashing*: side-channel surface. AWS and Aurora DSQL explicitly avoid this.
- *Live VM migration via FC*: not productionally supported by the FC project.

**Migration**:
- No migration; rebuild all images. Greenfield.

---

## Verification

End-to-end correctness checks per phase + an integrated final pass.

**Per-phase unit tests**:
- Phase 1: `engram-chunk-store` unit tests against in-memory + local-fs BlobStorage. Round-trips, GC correctness, cache eviction, manifest version monotonicity.
- Phase 2: image-builder produces a manifest + chunks; `chunk_store.materialize(manifest)` reproduces the original ext4 bit-for-bit.
- Phase 3: macOS — VZ backend creates a session against a chunked manifest, runs a process, snapshots, restores; output is identical.
- Phase 4: Linux — FC backend with NBD disk; integration test against `engram-sandbox-firecracker/tests/exec_real_vm.rs`-style harness.
- Phase 5: Memory restore timing test. Boot a canonical-snapshot image; measure time from `restore()` to `vCPU running`. Target: <100ms after warm-up. Run a workload that touches new memory; verify session-specific writes don't pollute the canonical chunk store entries.
- Phase 6: SandboxBackend trait shape; trait impl tests for all three backends.
- Phase 7: workspace builds with zero `tar.zst`, `flush`, `disk_pressure`, `SnapshotResidency` references.
- Phase 8: `helm install` against `kind` cluster + Postgres + a local BlobStorage works end-to-end.
- Phase 9: `terraform apply` in `deploy/terraform/gcp/examples/minimal/` against a real GCP project provisions the fleet; `engram session create` works.

**Integration tests**:
- Cross-host migration: in a 2-host setup, create a session on host A, force-migrate to host B, exec a command, verify output. Time target: <2s pause window.
- Spot preemption simulation: send `SIGTERM` to a host mid-session, verify drain hook fires, session migrates to surviving host.
- Session fork: `POST /sessions/abc/fork` produces a new session ID with identical disk/memory state. Both run independently after fork.
- Cross-session memory dedup: spin up 10 sessions from the same image; measure host RAM. Confirm canonical-memory is shared via page cache (not 10× duplication).
- Coordinator rolling restart: `kubectl rollout restart` mid-session; verify SSE reconnect via `Last-Event-ID`; verify no session impact.
- FC host rolling update: `gcloud compute instance-groups managed rolling-action start-update` with a new image; verify sessions migrate (don't go `Dead`); verify the drain hook fires in shutdown.

**Smoke**:
- `just dev` zero-config still works.
- `just check` passes per commit.
- Pre-existing demos in `docs/demo-firecracker-claude.md` continue to work.

---

## Out of scope (v2+)

- **AWS Terraform module** — contract documented, impl deferred. PR-able by community.
- **L2 regional shared cache** between hosts — single-cloud regional cache (Redis-style) for chunks. Optimization for large fleets.
- **Live memory migration** (no pause) — requires FC project support that doesn't exist productionally.
- **Cross-tenant page dedup** — KSM-style. Side-channel risk; defer indefinitely.
- **VZ memory snapshots on macOS** — VZ upstream is broken for arm64 guests; APFS-clone disk gives session-restart parity for dev.
- **Compression on chunks** — content-addressing already gives dedup. Add zstd-1 on cold-tier-only chunks later if storage cost demands it.
- **Hardware-accelerated decompression** (Intel IAA, Sabre-style) — Sapphire Rapids only; defer.
- **Multi-region active-active** — out of scope for v1 modules. Users deploy per-region instances and wire DNS.
- **KEK rotation primitive** — `:v1` hardcoded; flagged in `docs/known-issues.md`.

---

## Sequencing notes

Phases 1-5 are sequenced (each depends on the prior). Phases 6-7 can interleave or run in parallel with Phase 5's wrap-up. Phases 8-10 can start once Phase 6 lands (the deploy artifacts depend on the new APIs being stable). Realistically each phase is a few weeks of work; total project is several months of focused engineering.

If broken across multiple engineers: Phases 1-5 (the chunk store + adapters + image-builder + UFFD) must serialize because they share data structures. Phases 6 (trait refactor) and 7 (deletion) serialize after Phase 5. Phases 8 (Helm) and 9 (Packer + Terraform) can run in parallel by different engineers once Phase 6 lands. Phase 10 (ADR + docs) lands alongside Phase 9.
