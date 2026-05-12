# Chunked storage + production deploy — rollout tracker

Live punch list for the ADR 0007 chunked-immutable-storage migration
and the production-deploy work it gates. Keep this current as commits
land; if something here drifts from the code, the code wins —
update the doc.

The architectural plan lives in
[`docs/chunked-storage-plan.md`](./chunked-storage-plan.md) — the
phase-by-phase design + rationale. This file is the operational view:
what's shipped, what's left, what's blocking real deploy.

## Status legend

- ✅ **shipped** — landed on main, tests passing
- 🟡 **partial** — meaningful work landed, named gaps remain
- ⬜ **pending** — not started
- ⛔ **blocked** — needs design call or upstream dependency
- 💤 **deferred** — explicitly out of v1 scope

## Where we stand (one-paragraph)

All ten phases shipped to the engineering bar. Phase 6's destructive
trait reshape (snapshot/restore signatures, WIRE_VERSION v4) landed
alongside the additive surface (disk + memory manifest refs on
`SnapshotMetadata`). The standalone host-agent is wired end-to-end
(blob, chunk store, chunk cache, image cache, OCI auth via WS-RPC).
The remaining open work is **observability** (no metrics on the
chunked path; needs a Prometheus-vs-OTel framework call),
**validation gates** (real `helm install` + `terraform apply`
against a live cloud), and **the CI runner gap for the NBD test**
(Blacksmith's kernel doesn't ship `nbd.ko`, so the test runs locally
on the dev VM but skips on CI — see Phase 4). Everything else is
either deferred-with-rationale (AWS Terraform, L2 cache, KEK
rotation, live migration) or doc refresh (DESIGN.md narrative
still reflects ADR 0005's two-tier model).

---

## Phase 1 — Chunk store foundations

**Status: ✅ shipped — modulo S3 stub + schema-version doc note**

### Shipped

- `4a87cd6` scaffold + manifest format + ChunkStore API
- `bace991` local NVMe cache with LRU, pinning, singleflight
- `e3b3699` file ↔ manifest helpers (`chunk_file`, `materialize_to_file`)
- `4510abb` `BlobStorage::list_prefix` + GC + manifest version lookup
- `d0b8126` GCS pagination + cross-session e2e tests against
  fake-gcs-server

### Remaining

- ✅ **GC scheduler** — shipped as `engram_coordinator::chunk_gc`.
  Background loop fires `chunk_gc::run_once` on the
  `ENGRAM_CHUNK_GC_INTERVAL_SECS` cadence (default 1h); `0` disables.
  Retention via `ENGRAM_CHUNK_GC_RETAIN_SECS` (default 24h). The
  `POST /api/admin/gc-chunks` admin endpoint now delegates to the
  same pipeline, so explicit-trigger and cron-driver exercise
  identical code — including the disk+memory live-set union
  (latent bug fix from the original admin endpoint, which only
  read `list_live_disk_manifest_ids` and would have prematurely
  swept memory chunks).
- 💤 **`list_prefix` on S3** — stub at
  `crates/engram-storage-s3/src/lib.rs` returns `Err(Config)`.
  Breaks GC on AWS. Fix when AWS lands.
- ✅ **Local cache budget config** — shipped via
  `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` env var (commit `3d75723`).
  Both coord (`--mode=all`) and the standalone host-agent build
  the cache via `ChunkCacheConfig::from_env_or_default`. Terraform
  `fc-host-mig` module exposes a `chunk_cache_budget_bytes` var
  for per-fleet overrides.
- ⬜ **Schema-version-migration story** — manifests are
  `schema_version: 1`. No documented path for v2. One-line ADR note
  is enough for now; the writer rejects v≠1 already.

---

## Phase 2 — Image-builder chunked artifacts

**Status: ✅ shipped — modulo Directory-format note**

### Shipped

- `22efdc3` ext4 bake chunks the disk + writes a versioned manifest
  + drops a `bundle.json` sidecar with the manifest ref
- `1d8afbe` OCI push carries `bundle.json` as a third layer
  (`application/vnd.engram.bundle.v1+json`); host-agent's image_cache
  parses + surfaces it as `CachedImage.bundle`
- ✅ **image-builder GCS path** —
  `engram_image_builder::blob::from_env` mirrors coord + host-agent
  selectors. Reads `ENGRAM_BLOB_BACKEND={local,gcs}` +
  `ENGRAM_GCS_BUCKET`; production CI bakes land chunks directly in
  the deployment bucket without an intermediate local-to-GCS hop.
- ✅ **OCI push skips the rootfs.ext4 layer when bundle.json is
  present** (commit `d79094f`). `OciClient::push_image` takes
  `rootfs_ext4: Option<&Path>`; image-builder passes `None` when
  the bake produced a chunked disk manifest. Saves 4 GiB of
  registry bandwidth per push. Wire-redundant rootfs duplication
  retired.
- ✅ **Bake-time canonical memory capture** —
  `BuildRequest.capture_canonical_memory` opt-in (commit `0c88381`).
  Reuses `FirecrackerBackend::create + snapshot` to boot the
  just-built rootfs, capture `memory.bin`, chunk it. CI-gated
  integration test at `engram-image-builder/tests/canonical_capture.rs`.

### Remaining

- ⬜ **No working-set trace capture during bake** — UFFD handler
  captures traces at runtime; bake-time prefault-trace capture
  would let first-fault on a fresh host benefit too. Phase 5
  territory; not on the critical path.
- 💤 **`Format::Directory` bakes don't get chunked** —
  ProcessBackend is dev-only on macOS; acceptable not to chunk
  directories. Document this in the OCI bundle README so operators
  don't expect chunks on Directory bakes.

---

## Phase 3 — Disk adapter for macOS (VZ + Process)

**Status: ✅ shipped**

### Shipped

- `d6a6281` host-agent `PooledBackend.create()` materializes
  `cached.bundle.disk_manifest` → content-addressed file under
  `<materialize_dir>/<manifest_id>-vN.ext4` → `spec.rootfs_source`.
  VZ's existing APFS-clonefile per-sandbox flow operates on top.
- `e68ee23` VZ's `snapshot()` chunks the cloned rootfs into the
  store and populates `SnapshotMetadata.disk_manifest`.
- ✅ **Materialized file orphan reap** — shipped as
  `engram_host_agent::orphan_reap::reap_materialize_dir` +
  `POST /api/admin/reap-materialize-dir`. `engram_coordinator::chunk_gc`
  cron drives it; multi-host fanout via WS-RPC shipped in commit
  `2971115` (WIRE v3 / `HostAdminHandler::reap_materialize_dir`).
- ✅ **Chunk cache wired into materialize path** —
  `PooledBackend::with_chunk_cache` routes chunk reads through the
  NVMe LRU on rematerialize. Default 200 GiB budget,
  operator-tuned via `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`.
- ✅ **Standalone host-agent wiring** — see cross-cutting section
  below; shipped across `80d6841` (blob/chunk-store/image-cache
  env wiring) + `ad13dc0` (OCI auth via WS-RPC).

### Remaining

- 💤 **ProcessBackend chunked-rootfs path** — plan says "chunks
  become a cwd". Requires chunking Directory-format bakes. Low value
  (dev-only); deferred.

---

## Phase 4 — Disk adapter for Linux + FC (NBD daemon)

**Status: 🟡 partial — daemon + integration test shipped; CI runner
gap remains (Blacksmith kernel lacks `nbd.ko`)**

### Shipped

- **NBD wire codec + `ChunkedDiskBackend`** (commit `55dd889`,
  ~600 lines). Pure-Rust, target-agnostic data plane:
  `NbdRequest::parse` / `NbdReply::encode` (28-byte / 16-byte
  big-endian headers); `ChunkedDiskBackend::read` fans out across
  16 MiB chunk boundaries; writes copy base into a dirty buffer;
  `flush()` hashes + uploads + atomically rebases. 18 unit tests
  (codec + data plane) + a regression test for the
  post-flush-stale-read bug.
- **Linux NBD runtime** (commit `c770b6f`, ~500 lines). Direct
  kernel ioctls (`NBD_SET_SOCK` / `NBD_SET_BLKSIZE` /
  `NBD_SET_SIZE_BLOCKS` / `NBD_SET_FLAGS` / `NBD_DO_IT` /
  `NBD_DISCONNECT` / `NBD_CLEAR_SOCK`) — no external
  `nbd-client` binary required. `socketpair(AF_UNIX)` one half
  to the kernel; tokio task serves the other. Dedicated OS
  thread holds `NBD_DO_IT` for the lifetime of the device.
  Drop tears down deterministically (disconnect → join → clear
  → close). 5 `unsafe` blocks, each `// SAFETY:` annotated;
  host-agent crate-level `unsafe_code = "allow"` (was `forbid`).
- **`NbdSlotAllocator`** (commit `645afd8`). Async pool of
  `/dev/nbdN` device paths; `acquire()` waits when empty;
  `NbdSlot::Drop` returns the path. Duplicate-path rejection
  loud at construction. 5 unit tests.
- **`attach_manifest` + `NbdSandboxState`** (commit `e4f7500`).
  Composer: manifest → backend → claim slot → spawn daemon →
  wrap in state struct. Field-order-correct Drop (kernel sees
  disconnect before slot is reusable).
- **`PooledBackend` integration** (commit `94e9a52`). New
  `with_nbd_pool(allocator)` builder; `nbd_sandboxes: DashMap
  <SandboxId, NbdSandboxState>` tracks per-sandbox daemons.
  `resolve_rootfs` picks NBD over materialize-to-file when all
  prerequisites align. `destroy` removes from the map (Drop
  cleans up); `snapshot` calls `state.backend.flush()` BEFORE
  `inner.snapshot` so the new disk_manifest version is durable
  before FC's memory capture. Propagates onto
  `SnapshotMetadata.disk_manifest`.
- **Coord-side + host-agent-side env wiring** (this slice).
  `ENGRAM_NBD_DEVICES=/dev/nbd0,/dev/nbd1,...` env var on
  `--mode=all` coordinator startup AND on standalone
  `engram-host-agent` startup → `NbdSlotAllocator` → plumbed
  into the wrapping `PooledBackend`. Empty / unset → fall back
  to materialize-to-file (the existing chunked path).

### Still pending

- ⬜ **CI runner for the NBD test.** The
  `crates/engram-host-agent/tests/nbd_chunked_disk.rs` test
  exists and runs locally on the dev VM, but Blacksmith's runner
  kernel doesn't ship `nbd.ko` (custom guest kernel, no Ubuntu
  apt package matches). The CI step (`Detect NBD kernel module`
  in `ci.yml::test-firecracker`) detects the gap and emits a
  `::warning::` annotation; the test is gated on the detection
  output, so it skips rather than fakes a pass — but that
  violates the "new tests must run in CI" rule. The right
  follow-up is wiring the dev VM as a GH self-hosted runner
  for this one test (the rest of the FC suite already works on
  Blacksmith).

### Dependencies / open questions

- Kernel must have `CONFIG_BLK_DEV_NBD=y` (Ubuntu cloud image —
  yes; production base image — verify in the Packer manifest).
- No `nbd-client` userspace required — the daemon talks the
  kernel ioctls directly.

---

## Phase 5 — Memory adapter (UFFD-from-chunks + canonical + WS R&R)

**Status: ✅ shipped**

### Shipped

- **`engram-uffd-handler::chunked`** (`2e40f0b`) — pure-Rust data-
  plane resolver. `ChunkedMemoryBackend` pre-parses canonical +
  session manifests into positional arrays for O(1) per-fault
  `resolve(byte_offset)`. `ResolvedPage::Canonical { offset }`
  → copy from mmap; `ResolvedPage::Chunk { hash }` → fetch via
  ChunkCache. 8 unit tests.
- **`engram-uffd-handler::working_set`** (`2ac8ecd`) —
  `WorkingSetRecorder` accumulates session-divergent chunk hashes
  inside a fixed time window (5s default), deduped + ordered, ready
  to publish as `traces/<manifest_id>/<host_id>.json`. 6 unit tests.
- **Runtime rewrite + new CLI** (`34310a2`) — `Runtime::new` takes
  a canonical path + `Arc<ChunkedMemoryBackend>` + a tokio Handle.
  Per-fault: round to chunk boundary, resolve, install the **full
  512 KiB chunk** in one UFFDIO_COPY (amortises the 128-page cost).
  Pre-install bitmap guards double-install races; UFFDIO_WAKE
  fallback for already-installed pages. `prefault_from_trace`
  pre-installs every position in the session manifest before vCPUs
  unfreeze. CLI: `--listen --canonical-memory --canonical-manifest
  --session-manifest [--prefault-trace --publish-trace-host
  --cache-root --cache-budget-bytes --recorder-window-ms]`. Blob
  backend via `ENGRAM_BLOB_BACKEND={local,gcs}`.
- **Postgres + migration 0019** (`e3222bd`) — `memory_manifest_id`
  + `memory_manifest_version` on `snapshots` mirroring 0018; both-
  or-neither CHECK constraint + partial GC index.
  `MetadataStore::list_live_memory_manifest_ids` is the symmetric
  GC primitive. `SnapshotMetadata.memory_manifest` +
  `SnapshotRecord.memory_manifest` thread through every backend
  and the coordinator API.
- **FC backend wiring** (`ee444ff`) — `FcSnapshotManifest` gains
  `memory_manifest` + `canonical_memory_manifest`. UFFD restore
  refuses without `memory_manifest` (no silent fallback to a
  degenerate path); `canonical_memory_manifest` defaults to
  `session_manifest` (resolver returns `Canonical` for every
  fault, mmap'd memory.bin serves the bytes — no chunk-store I/O
  at runtime until bake-time canonical capture lands).
- **`PooledBackend::snapshot` chunked wrap** (this slice) —
  intercepts FC's bare snapshot, calls
  `ChunkStore::chunk_file(memory.bin, Memory)`, commits a fresh
  `ManifestRef`, patches `manifest.json`'s `memory_manifest` field
  (JSON-level patch — no FC-internal struct exposed), and updates
  `SnapshotMetadata.memory_manifest`. Two unit tests: positive
  (memory.bin → chunks → byte-equal materialize) + no-chunk-store
  passthrough.

### Phase 5 follow-up: cross-host trace + memory.bin materialization (shipped)

- ✅ **`HostId` plumbed through FC backend.**
  `FirecrackerConfig.host_id: Option<HostId>` set at construction
  in both host-agent main (per-startup HostId::new) and
  coord-side --mode=all (stable in-proc `00000000-0000-4000-
  8000-000000000a11`). Hoisted the stable id to top of coord
  main so the FC backend stamps it on its config before being
  wrapped.
- ✅ **`trace_host_hint` on FcSnapshotManifest.** Snapshot stamps
  the current host's id; cross-host restore reads it and passes
  to `spawn_uffd_handler` as `--prefault-trace <hint>`. The
  recorder publishes the new trace under THIS host's id via
  `--publish-trace-host <current>`, so subsequent restores on
  the same host use a local recording (and cross-host restores
  reuse the original host's trace).
- ✅ **Cross-host memory.bin materialization** in
  `PooledBackend::restore`. When the snapshot dir's `memory.bin`
  is missing locally but the sidecar JSON's `memory_manifest`
  is set, rebuild the file from chunks via
  `ChunkStore::materialize_to_file_cached` before delegating
  to inner.restore. Two test cases:
  - `restore_materializes_missing_memory_bin_from_chunks` —
    positive (1 MiB synthetic memory bin, byte-equal recovery).
  - `snapshot_skips_materialize_when_memory_bin_already_present`
    — negative (file untouched when locally present).

### Phase 5 follow-up: image-builder bake-time canonical capture (shipped)

- ✅ **`BuildRequest.capture_canonical_memory`** opt-in flag +
  `CanonicalCaptureConfig { kernel_image_path, firecracker_bin,
  boot_wait, memory_mib }`. When set, `Builder::capture_canonical_memory`
  reuses `FirecrackerBackend::create + snapshot` to boot the
  just-built rootfs, wait for steady state, capture `memory.bin`,
  and chunk it into the chunk store. Best-effort failure
  semantics: a capture failure (no KVM, FC binary missing, VM
  panic) logs + leaves canonical_memory_manifest=None — the bake
  still ships a valid disk-only image.
- ✅ **`bundle.json::canonical_memory_manifest`** field plus
  matching `ImageBundle.canonical_memory_manifest` deserializer
  with `#[serde(default)]` for backwards compat. The
  PooledBackend.create path already lifts the bundle's canonical
  ref onto `SandboxSpec.canonical_memory_manifest` (Phase 5 slice
  1); FC backend then stamps it on `FcSnapshotManifest.canonical_memory_manifest`
  at snapshot time. UFFD handler reads it via `--canonical-manifest`
  at restore.
- ✅ **`engram-sandbox-firecracker` dep on image-builder is
  dev-only** — image-builder can take a real dep on FC for this
  capture path without a cycle.
- ✅ **Linux+KVM integration test**:
  `crates/engram-image-builder/tests/canonical_capture.rs` —
  preflight-skipped on non-KVM lanes, wired into CI under
  `test-firecracker` via `cargo nextest run -p
  engram-image-builder --test canonical_capture --run-ignored
  ignored-only`.
- ✅ **Production wiring**: Packer manifest sets
  `nbds_max=64` in `/etc/modprobe.d/engram-nbd-tuning.conf`;
  Terraform `fc-host-mig` module's startup-script writes
  `ENGRAM_NBD_DEVICES=/dev/nbd0,...,nbd<N-1>` into
  `/etc/engram/host-agent.env` based on a new `nbd_slots`
  variable (default 16). The host-agent reads the env at startup
  and wires the slot pool into PooledBackend.

### Dependencies / open questions

- Kernel CONFIG_USERFAULTFD=y (Ubuntu cloud image — yes)
- Cross-session canonical sharing relies on host page cache, which is
  fine on Linux but doesn't translate to macOS / VZ — VZ never gets
  memory chunking in v1 per the plan (its native memory-snapshot is
  upstream-broken anyway)

---

## Phase 6 — SandboxBackend trait + MetadataStore refactor

**Status: ✅ shipped — additive surface AND destructive trait reshape**

### Shipped (additive)

- `e68ee23` `SnapshotMetadata.disk_manifest: Option<ManifestRef>`,
  populated by VZ snapshot, threads through the trait without
  breaking signatures.
- `ManifestRef` hoisted to `engram-core::types::manifest` to break
  the dep cycle (chunk-store depends on engram-core for
  BlobStorage; engram-core can't depend back).
- `0df3a31` `WIRE_VERSION` constant + hello-frame handshake.
  Production-grade version negotiation; bumped to v2 in `ad13dc0`
  for the `ResolveRegistryAuth` add.
- **DB migration `0018_chunked_storage.sql`** —
  `disk_manifest_id` + `disk_manifest_version` columns on
  `snapshots`. **Additive**, not the full Phase 6 drop: the
  cold-tier columns stay until Phase 7 deletion lands. The
  `idx_snapshots_disk_manifest` partial index supports the GC
  sweep's live-set query. CHECK constraint rejects half-populated
  rows.
- **`SnapshotRecord.disk_manifest`** persists through
  `MetadataStore::record_snapshot` + reads back via
  `list_snapshots_for_session` /
  `latest_snapshot_for_session`. Live-PG round-trip locked in by
  `crates/engram-coordinator/tests/snapshot_disk_manifest_persistence.rs`
  (gated on `ENGRAM_TEST_DATABASE_URL`; runs in CI via the
  Postgres-gated step).
- Coordinator's snapshot recording paths (`api/snapshot.rs`,
  `idle_evictor.rs`) propagate `metadata.disk_manifest` into the
  row, so VZ snapshots taken via the chunked write path
  (commit `e68ee23`) now persist their manifest ref end-to-end.

### Shipped (destructive trait reshape)

- ✅ **`SandboxBackend::snapshot(id)`** — dropped `dest: &Path`. The
  backend chooses its own per-snapshot staging dir
  (`<work_dir>/snapshots/<snapshot_id>/`); coord no longer dictates
  layout. Aligns with ADR 0007's "chunks are the cross-host
  durability primitive; local files are a per-host cache."
- ✅ **`SandboxBackend::restore(metadata: SnapshotMetadata)`** —
  dropped `src: PathBuf`. The backend resolves
  `snapshot_path_for(metadata.id)` for its own local lookup;
  PooledBackend wraps to materialise `memory.bin` from chunks
  when the local file is missing (cross-host migration case).
- ✅ **`snapshot_path_for(snapshot_id)` accessor** added to the
  trait so PooledBackend (the only legitimate host-side caller)
  can read/patch `memory.bin` + `manifest.json` after the inner
  backend returns. RemoteSandboxBackend / HostRegistry impls
  return a sentinel that fails loudly on any actual file I/O —
  coord callers should never reach into a remote host's
  filesystem layout.
- ✅ **`WIRE_VERSION` v3 → v4** for the
  `RequestKind::{Snapshot, Restore}` wire-shape change.
  `RequestKind::Snapshot { sandbox_id }` (no `dest_path`);
  `RequestKind::Restore { metadata: SnapshotMetadata }` (no
  `src_path`).
- ✅ **Coord callers simplified**: `api/snapshot.rs` and
  `idle_evictor.rs` no longer pre-allocate staging dirs;
  `restore_for_session` takes a `SnapshotMetadata`. The
  resume path's pre-flight "manifest gone on disk" check
  retires — backend.restore now surfaces a typed
  `SandboxError::Snapshot` that the coord maps to 410 Gone.

### Re-scoped / not done

- 💤 **`SandboxSpec.rootfs: ManifestRef`** — kept as
  `rootfs_source: Option<PathBuf>`. PooledBackend.resolve_rootfs
  centralises manifest → path resolution; making each backend
  resolve its own manifest would duplicate that logic three
  ways (FC, VZ, Process). The current host-local PathBuf is
  the right abstraction.
- 💤 **`memory_canonical` field on SandboxSpec** — already shipped
  as `canonical_memory_manifest` (Phase 5).
- 💤 **`working_set_trace` field on SandboxSpec** — host-scoped +
  looked up at restore time via `metadata.trace_host_hint`; not
  load-bearing on the create spec.
- ✅ **Migration `0019_drop_cold_tier_columns.sql`** — shipped in
  Phase 7 as `0020_drop_cold_tier.sql`; the cold-tier columns
  were already gone before this slice.
- ✅ **`SnapshotResidency` enum / `flush_to_cold` /
  `clear_local_path` / `latest_cold_snapshot_for_session`** —
  shipped in Phase 7.
- ✅ **`ensure_active` simplification** — shipped in Phase 7.

---

## Phase 7 — Delete v1 tar.zst path

**Status: ✅ shipped**

Pure deletion. Cold-tier flush pipeline retires; chunked manifests
become the single durability primitive.

- ✅ `crates/engram-host-agent/src/flush.rs` — deleted (324 lines)
- ✅ `crates/engram-host-agent/src/disk_pressure.rs` — deleted
  (399 lines)
- ✅ `crates/engram-coordinator/src/blob.rs` — slimmed to just
  `from_env`; `seal_blob_ref` / `open_blob_ref` / `snapshot_blob_key`
  / `unpack_blob_to_dir` deleted. `engram-crypto::CredCipher`
  itself stays — `registry_credentials` + `session_secrets` are
  still envelope-encrypted.
- ✅ `crates/engram-coordinator/src/api/admin.rs::flush_one`,
  `flush_idle` + their wire route — deleted. `FlushResult` +
  `flush_inner` deleted. The admin surface is now
  `gc_chunks` + `reap_materialize_dir`.
- ✅ `start_coordinator` — `disk_pressure::spawn` block gone.
- ✅ `SnapshotEvent::ColdEvicted` + `SnapshotEvent::ColdResumed` —
  deleted from `engram-coordinator::state`.
- ✅ `SessionStatus::ColdEvicted` — deleted from
  `engram-core::types::session`.
- ✅ `MetadataStore::flush_to_cold`, `clear_local_path`,
  `latest_cold_snapshot_for_session`, `list_idle_sessions` —
  trait methods deleted. `SealedBlobRef` deleted.
- ✅ `SnapshotRecord.local_path`, `blob_present`, `replicated_at` —
  deleted; `SnapshotResidency` deleted. The Coord-side
  `ensure_active` simplifies from a three-branch dispatcher to
  Idle/Dead; `resume_from_cold` gone.
- ✅ **Migration `0020_drop_cold_tier.sql`** drops the columns +
  rebuilds `sessions_status_check` without `'cold_evicted'`.
  `wrapped_dek`, `nonce`, `ciphertext`, `key_id`, `blob_present`,
  `replicated_at`, `local_path` columns dropped from
  `snapshots`. `cold_evicted_at` dropped from `sessions`.
- ✅ Test fixtures (`cold_tier_round_trip.rs`, FC `cold_tier.rs`)
  deleted. Remaining mock impls updated to drop the cold-tier
  methods.

**Non-trivial follow-up landed alongside**: same-host resume now
reconstructs the snapshot dir from
`(cfg.local_path, session_id, snapshot_id)` instead of reading
`record.local_path`. The snapshot API renames the staging dir to
`<snapshot_id>` after the backend returns. Idle-evictor mirrors
the rename. Cross-host materialization (rehydrate dir from chunks
when the picked host doesn't have it) is task #33.

---

## Phase 8 — Helm chart for coordinator

**Status: 🟡 partial — chart shipped; multi-cluster validation pending**

`deploy/helm/engram-coordinator/`. Cloud-agnostic templates;
cloud-specific values in `values-gcp.yaml.example` /
`values-aws.yaml.example`.

- ✅ `Chart.yaml`, `values.yaml`, templates for `deployment`,
  `service`, `serviceaccount`, `configmap`, `hpa`, `pdb`,
  `networkpolicy`, `ingress`, `_helpers.tpl`, `NOTES.txt`.
  `helm lint` clean; `helm template` renders against both
  `values-gcp.yaml.example` and a minimal local-dev overlay.
- ✅ README documenting prerequisites + values reference + GKE
  + kind quickstarts.
- ⬜ Validation against a real `kind` cluster (cloud-agnostic) +
  a real GKE cluster. Rendering works; runtime healthcheck
  pending.
- 💤 `secretproviderclass.yaml` — Secrets Store CSI Driver is
  cloud-specific (GCP CSI vs AWS Secrets Store CSI) and depends
  on the driver being installed cluster-side. Operators add it
  per their cluster shape; chart stays driver-agnostic.

---

## Phase 9 — Packer + GCP Terraform

**Status: 🟡 partial — Packer + 3 core Terraform modules shipped; GKE/Postgres/SecretManager/AR deferred to operator-provided modules**

`deploy/packer/` + `deploy/terraform/gcp/`. The deliberate split:
own the pieces specific to Engram (chunks bucket, FC host MIG,
host image build); defer the well-trodden pieces (GKE cluster,
Cloud SQL Postgres, Artifact Registry) to Google's published
modules. Reasoning in `deploy/terraform/gcp/README.md`.

- ✅ **Packer manifest** at `deploy/packer/fc-host-gcp.pkr.hcl`
  + provisioners. Builds a GCE image with Firecracker + the
  static-musl `engram-host-agent` binary + systemd unit + the
  drain hook (`engram-drain.sh`). Pins FC version + pulls the
  binary from a GCS URL the operator's CI populates.
- ✅ **Terraform modules** (`network`, `storage`,
  `fc-host-mig`) + `examples/minimal/` wiring them. `terraform
  validate` + `terraform fmt -check` clean. The example
  outputs the values the Helm chart's values.yaml needs
  (chunks bucket name, KEK resource path, coordinator SA email).
- ✅ **Infrastructure contract** in
  `deploy/terraform/gcp/README.md` — what Engram needs from the
  cloud + what operators bring themselves.
- ⬜ Real `terraform apply` against a live GCP project (have
  validated HCL syntax + module wiring; haven't actually
  provisioned the fleet).
- 💤 `deploy/packer/fc-host-aws.pkr.hcl` + Terraform AWS module
  — contract documented, impl deferred.
- 💤 `modules/gke`, `modules/postgres`, `modules/secrets`,
  `modules/artifact-registry`, `modules/workload-identity` —
  use Google's published modules instead. The example
  documents which ones.

**Operator-followup blocker**: per the example's README, the
Helm chart needs the coord's `SQL` URL, `auth tokens`, and the
internal LB URL of the coord. The Terraform reference produces
the IAM scaffolding so a Helm install can chain on top, but the
operator must (a) run a GKE module separately, (b) populate
Secret Manager entries with the runtime values, (c) feed the
coord LB URL back as `coordinator_endpoint` in a second
`terraform apply`. Documented in the README's "Apply order".

**Critical: this phase can NOT compensate for missing Rust wiring.**
Setting env vars in a Packer manifest only matters if the binary
reads them. The host-agent's Tier 3 wiring (commit `80d6841`)
+ the WS-RPC auth (`ad13dc0`) are prerequisites for the Packer
image actually doing anything useful.

---

## Phase 10 — ADR 0007 + doc updates

**Status: 🟡 partial — ADR + known-issues + intro callouts shipped; deep narrative refresh pending Phase 6**

- ✅ [`docs/adr/0007-chunked-immutable-storage.md`](./adr/0007-chunked-immutable-storage.md)
  — the headline ADR. Context + decision (chunk sizes, manifest
  shape, memory dedup, working-set traces, layered architecture,
  COW levels, cold-tier deletion, wire protocol), consequences,
  alternatives considered, rollout pointer, explicit "what this
  ADR does NOT cover" section listing the deferred pieces.
- ✅ `docs/known-issues.md` — six new entries (#9 NBD, #10 UFFD,
  #11 schema reshape blocked, #12 observability, #13 materialize-
  dir leak, #14 wire-version bincode caveat) all cross-referenced
  to the rollout doc's Tier 4 punch list. Existing entries
  untouched (none were affected).
- ✅ `README.md` — Phase 7 paragraph added; the "two snapshot
  tiers, one primitive" architecture paragraph replaced with
  the chunked-storage narrative + a pointer to ADR 0007 +
  rollout doc.
- ✅ `DESIGN.md` — ADR 0007 added to the ADR list with the
  "supersedes 0005's two-tier framing" note; deploy artifact
  pointers added; intro callout explains that the ADR-0005-era
  narrative below is historical pending Phase 6 schema reshape.
- ✅ `docs/deploy.md` — header callout supersedes the cold-tier
  framing + points at the deployment artifacts (Helm, Packer,
  Terraform) and the rollout tracker.
- ⬜ **Deep rewrite of DESIGN.md's architecture sections** —
  source-of-truth table, snapshot-residency diagrams,
  hot/cold/tier component descriptions all still reflect ADR
  0005. Refreshes naturally with Phase 6's `SnapshotRecord`
  reshape (the new fields drive the new descriptions).
- ⬜ **Deploy.md rewrite** — env vars + topology section is
  still accurate, but the storage narrative is ADR-0005-era.
  Refresh alongside Phase 6.

---

## Cross-cutting: Standalone host-agent wiring

**Status: ✅ shipped**

The `engram-host-agent` binary (used in `--mode=host` multi-host
production) now wires ImageCache, ChunkStore, ChunkCache, BlobStorage,
and OCI auth end-to-end.

### Shipped

- ✅ `engram_host_agent::blob::from_env()` mirrors
  `engram_coordinator::blob::from_env`. Reads
  `ENGRAM_BLOB_BACKEND={local,gcs}` + `ENGRAM_GCS_BUCKET`.
- ✅ Host-agent main.rs constructs `ChunkStore` + `ChunkCache` +
  materialize_dir at `<work_dir>/`, wires
  `HostAgent::with_chunk_store(...)` / `with_chunk_cache(...)` /
  `with_image_cache(...)`.
- ✅ **OCI auth via WS-RPC** — the previously open design
  question resolved in option (2). `WsAuthResolver` in the
  host-agent issues `RequestKind::ResolveRegistryAuth` over the
  existing dialer WebSocket; the coord delegates to
  `engram-oci-auth::PgAuthResolver`. Plaintext creds traverse the
  WS only at pull time; never persisted on the host. `WIRE_VERSION`
  bumped to v2 for the new RPC variant (now v4 after subsequent
  bumps). Commit `ad13dc0`.

---

## Cross-cutting: Observability

**Status: ⬜ pending**

Zero metrics on the chunked-storage code paths today. Before
production traffic, need:

- ⬜ Cache hit/miss counters on `ChunkCache`
- ⬜ Chunk fetch latency histogram (split by local-NVMe hit vs
  BlobStorage round-trip)
- ⬜ Materialization time per (manifest, host)
- ⬜ GC counts: chunks deleted, manifests retained
- ⬜ Manifest put/get rates
- ⬜ Snapshot chunking time (VZ today, FC + UFFD when those land)

Format / framework: match whatever the rest of the stack uses.
Today the codebase emits structured `tracing` logs but doesn't appear
to expose Prometheus. Need a call on whether to add a metrics
exporter at this point.

---

## Cross-cutting: Wire compatibility

**Status: ✅ shipped**

- ✅ `engram_protocol::WIRE_VERSION` constant + hello-frame check.
  Currently at **v4**:
  - v1 — `SnapshotMetadata.disk_manifest` add (`e68ee23`).
  - v2 — `RequestKind::ResolveRegistryAuth` for OCI auth WS-RPC
    (`ad13dc0`).
  - v3 — `RequestKind::ReapMaterializeDir` + `WireReapStats` for
    multi-host materialize-dir reap fanout (`2971115`).
  - v4 — Phase 6 destructive trait reshape:
    `RequestKind::Snapshot` drops `dest_path`; `Restore` takes
    `metadata: SnapshotMetadata` instead of `src_path` (`b8afb42`).
  The host-agent's `Hello` frame ships its `WIRE_VERSION`; the
  coord rejects on mismatch in `api/hosts.rs::handle_connection`.
  Mixed-version deploys are refused loudly with a wire-version
  log line on both sides. Future contributors editing
  `engram-protocol::wire` should bump the version + add a history
  note alongside any structural change.

---

## Maturity tiers — the route to production

Four tiers of validation, each gating the next. Don't skip ahead;
each tier surfaces failure modes the next can't.

```
T1 (Mac local) → T2 (dev-vm Linux) → T3 (coord + host split) → T4 (GCP prod)
```

Within each tier, the **required work** list is the minimum to claim
that tier. **Exit criteria** is the executable check that says "done."
Each tier inherits everything proven in the prior tier.

---

### Tier 1 — Can test locally on macOS (`--mode=all`)

**Goal**: bake an image → start coord → create session → exec
something → snapshot → resume → exec again, with the chunked path
demonstrably involved.

**Entry criteria**: macOS dev box, `just dev` brings up the stack
cleanly.

**Current state**: mostly there. `--mode=all` wiring shipped in
`d6a6281` + `e68ee23`. Unit tests cover the pieces. **What's
missing is the integrated scenario.**

**Required work**:

- ✅ **End-to-end chunked-lifecycle test through `PooledBackend`**
  in `crates/engram-host-agent/src/pooled_backend.rs::tests::create_with_image_uri_resolves_chunked_path_on_inner`.
  Asserts: bundle.json parsing, chunk-store materialization,
  `spec.rootfs_source` rewrite, inner backend receives the
  materialized path, materialized bytes match the chunked source.
  Uses a `parking_lot`-backed capturing inner backend so the test
  is portable (no VM, no Docker).

Tier 1 entry was always close to true; the missing piece was a
test that locks in the `--mode=all` chunked wiring as a regression
guard. The pre-existing
`materialize_chunked_rootfs_round_trips_and_dedupes` test covers
the materialize helper in isolation; the new test extends to
`PooledBackend.create()` driving the helper through `ImageCache`.

No `just chunked-smoke` recipe — running an ad-hoc Docker bake
to assert what unit tests already prove was duplicate work. CI
runs the integration test; that's the durable signal.

**Exit criteria**:

```
cargo test -p engram-host-agent --lib \
    pooled_backend::tests::create_with_image_uri_resolves_chunked_path_on_inner   # passes
```

---

### Tier 2 — Can test with `dev-vm` on real Linux (FC + KVM)

**Goal**: the Tier-1 scenario, but with Firecracker microVMs on the
GCP Linux dev box via the `dev-vm` skill.

**Entry criteria**: Tier 1 green. dev-vm bootstrapped, kernel +
ubuntu rootfs cached
(`bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh`).

**Current state**: the existing FC integration tests (`exec_real_vm`,
`harness_loopback`, `proxy_e2e`) were updated to construct chunk
stores in their Builder calls (commits 22efdc3 + 1d8afbe), but they
haven't actually been run on Linux this session. There's a real
possibility a Linux-only compile path or a test-time assumption
broke.

**Required work**:

- ✅ **Existing FC test suite green on the dev VM.** All 7 heavy
  integration tests pass against real microVMs:
  `boot_microvm_and_capture_kernel_banner`,
  `create_list_destroy_round_trip`,
  `snapshot_then_restore_round_trips_microvm`,
  `snapshot_then_uffd_restore_round_trips_microvm`,
  `cold_tier_round_trip_on_real_microvm`,
  `exec_runs_inside_baked_microvm`,
  `noop_harness_round_trips_three_tool_calls_on_real_fc`.
  The last two go through the chunked image-builder + bake the
  rootfs as chunked output — proves the chunked bake is
  FC-bootable end-to-end.
- ✅ **`just fc-bake-demo` works.** Verified on dev-vm: bake
  produces `bundle.json` + 14 chunks under
  `store/chunks/sha256/...` + `manifests/<id>/v1.json`.
- ✅ **CI runs these tests.** `.github/workflows/ci.yml:343-356`
  invokes the heavy FC suite (unprivileged + root sets) on every
  push. The chunked changes flow through automatically.
- 💤 **No FC-specific `chunked_disk.rs` test.** The plan called
  for one; on reflection it would duplicate proof already given
  by (a) Tier 1's `PooledBackend` routing test (backend-agnostic)
  and (b) `exec_real_vm` / `harness_loopback` proving the chunked
  bake output is FC-bootable. Same duplicate-coverage lesson
  that retired the `just chunked-smoke` recipe.

**Exit criteria**:

```
/dev-vm run bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh all   # passes ✓
/dev-vm run just fc-bake-demo   # passes ✓
```

---

### Tier 3 — Can test coord + host split locally

**Goal**: coordinator and host-agent in separate processes (could
even be separate machines), communicating over the dialer WebSocket,
sharing a chunk store. This is the topology production runs.

**Entry criteria**: Tier 2 green.

**Current state**: wiring is shipped. The remaining gap is a
manual validation pass with two real processes.

**Required work**:

- ✅ **`engram_host_agent::blob::from_env()`** mirroring coord's
  selector. `local` (default) + `gcs`; fails closed on
  misconfiguration. Shipped in `80d6841` with unit tests.
- ✅ **Host-agent main.rs reads blob backend from env** + constructs
  `ChunkStore` + materialize_dir at `<work_dir>/chunked-rootfs/`.
- ✅ **`HostAgent::with_chunk_store(cs, dir)` call** threads through
  to PooledBackend during `run()`.
- ✅ **OCI auth (Tier 3 placeholder)** — anonymous resolver wired
  in `main.rs`. Works for `localhost:5001` testing + public
  registries. *Tier 4 needs a real credential story (see below).*
- ✅ **`ImageCache::open` call** in `main.rs` at
  `<work_dir>/oci-cache/`; `HostAgent::with_image_cache(...)`
  threads through to PooledBackend.
- ✅ **Wire-version handshake** — `engram_protocol::WIRE_VERSION`
  (v1) shipped in `0df3a31`. `NotifyKind::Hello.wire_version`
  exchanged on connect; coord's `/api/hosts/connect` rejects
  mismatch with a loud error.
- ⬜ **Two-process manual smoke.** The wiring is in; running it
  end-to-end as separate processes is the final validation. Skip
  if confident in the unit + integration coverage; do it if you
  want bit-level confidence before driving toward T4. Shape:
  ```
  # Terminal A (or process A)
  engram-coordinator --mode=coordinator --local-path=/shared ...
  # Terminal B (or process B)
  engram-host-agent --coordinator http://... --work-dir=... \
      ENGRAM_BLOB_BACKEND=local ENGRAM_LOCAL_PATH=/shared ...
  # Terminal C — session lifecycle via the API
  ```
  Both processes point at the same `ENGRAM_LOCAL_PATH` so chunks
  produced by the image-builder on one side are readable by the
  host-agent on the other. Production swaps both to GCS with the
  same bucket.

**Exit criteria**:

```
cargo test -p engram-host-agent --lib blob::tests::from_env_dispatches_per_backend_var   # passes ✓
cargo test -p engram-protocol --lib codec::tests::encode_decode_round_trips_notify       # passes ✓
# Optional: end-to-end two-process smoke per the shape above.
```

---

### Tier 4 — Productionization on GCP

**Goal**: real users on `cortex.<domain>`, served by Engram on GKE +
a GCE MIG of Firecracker hosts.

**Entry criteria**: Tier 3 green; Tier 3 surfaces any wire-level or
multi-process bugs before they're under load.

**Current state**: not even adjacent. Major Phases 4-10 + cross-
cutting work still pending. Below is the criticality-ordered work
list *within* Tier 4. None of it should start before Tier 3 is
solid — production has too many failure modes for a fragile lower
tier.

**Required work** (criticality-ordered):

1. ✅ **Image-builder GCS path** (was Phase 2 gap #5) — shipped:
   `engram_image_builder::blob::from_env` mirrors coord + host-
   agent selectors. Reads `ENGRAM_BLOB_BACKEND={local,gcs}` +
   `ENGRAM_GCS_BUCKET`; local default keeps existing dev workflows
   working. Production CI bakes now land chunks directly in the
   deployment bucket without an intermediate local-to-GCS hop.
2. ✅ **WS-RPC for OCI auth resolution on the standalone
   host-agent** — shipped in `ad13dc0`. The host-agent's `OciClient`
   uses a `WsAuthResolver` that issues
   `RequestKind::ResolveRegistryAuth` over the dialer WebSocket;
   coord delegates to the existing `engram-oci-auth::PgAuthResolver`
   (Static / GcpWorkloadIdentity / Anonymous auth_kinds all flow).
   Plaintext creds traverse the WS only at pull time. WIRE_VERSION
   bumped to v2 for the new enum variants.
3. **Materialized rootfs file GC** (Phase 3 gap) — two concerns:
     - ✅ *(a)* Local NVMe cache for chunks across manifests —
       `PooledBackend::with_chunk_cache` wires the existing
       `ChunkCache` so `materialize_to_file_cached` serves repeat
       reads from local disk. Default 200 GiB budget. Coord
       `--mode=all` + standalone host-agent both wire one. Test
       at `pooled_backend::tests::materialize_chunked_rootfs_uses_chunk_cache_when_present`
       proves the cache path is exercised (materialize succeeds
       even after the underlying store's chunks are deleted).
     - ✅ *(b)* Materialized-file orphan reap — shipped as
       `engram_host_agent::orphan_reap::reap_materialize_dir`
       primitive + `POST /api/admin/reap-materialize-dir` admin
       endpoint. Parses `<manifest_id>-vN.ext4` filenames; deletes
       any not in the live-set (same query the chunk GC uses).
       `min_age_secs` gate guards in-flight `create()` clonefiles.
       Multi-host fanout shipped: in `--mode=coordinator` the
       endpoint walks every connected host with an `admin_client`
       and calls `RequestKind::ReapMaterializeDir` (WIRE v3) so
       each host sweeps its own local dir. Per-host failures
       surface in `per_host[].error`; top-level totals sum
       successful runs. Hosts without a wired `HostAdminHandler`
       (no `materialize_dir` configured) skip gracefully with a
       typed error. Cron scheduler pairs with #4.
4. ✅ **Chunk-store GC scheduler** (Phase 1 gap) — shipped. Cron
   loop `engram_coordinator::chunk_gc::spawn` in `start_coordinator`
   + `POST /api/admin/gc-chunks` admin endpoint delegating to the
   same `run_once` pipeline. Disk+memory live-set union (admin
   endpoint previously only read disk). 8 unit tests.
5. 🟡 **Phase 6 trait reshape + migration 0018/0019** — additive
   surface shipped (`SnapshotMetadata.disk_manifest`,
   `.memory_manifest`); full trait reshape lands alongside Phase 7
   deletion.
6. ✅ **Phase 7 tar.zst deletion** — shipped. Cold-tier
   flush/disk-pressure/sealed-blob machinery removed; migration
   0020 drops the columns. Chunks are the only durability surface.
7. ⬜ **Observability** — cache hit rate, chunk fetch latency,
   materialize time, GC counts. Without these, debugging production
   slowness is guesswork. Probably a Prometheus exporter; needs a
   framework call.
8. 🟡 **Phase 4 NBD** — daemon + PooledBackend integration
   shipped (commits `55dd889`, `c770b6f`, `645afd8`, `e4f7500`,
   `94e9a52`). End-to-end FC microVM integration test (boot a
   real VM against `/dev/nbd0` served by the daemon) deferred
   to a focused dev-VM session. Wired behind
   `ENGRAM_NBD_DEVICES=…` so operators opt in.
9. ✅ **Phase 5 UFFD + canonical memory + WS R&R** — runtime +
   snapshot wiring shipped (bake-time canonical capture + cross-
   host trace/memory.bin materialization are tasks #32/#33).
10. ✅ **Phase 8 Helm** — chart shipped; multi-cluster validation
    pending.
11. ✅ **Phase 9 Packer + Terraform** — Packer + 3 core modules
    shipped; GKE/Postgres/SecretManager/AR deferred to operator
    modules per the contract doc.
12. ✅ **Phase 10 ADR + docs** — ADR 0007 + known-issues +
    DESIGN.md + deploy.md updated.

**Exit criteria**: a real user session on the GCP deployment runs
end-to-end with chunked storage, cross-host resume, and no manual
intervention.

---

## Next concrete step

Per the tier ladder, the next slice of work is **Tier 1 exit**: a
`just chunked-smoke` (or equivalent) + a focused integration test
that locks the `--mode=all` chunked lifecycle in place. That gives
us a regression guard before climbing to Tier 2.

---

## ADR 0008 migration — chunks-in-OCI (proposed, design phase)

**Status: 0 (design)** — `docs/adr/0008-chunks-in-oci.md`.

The architectural shift: OCI registry becomes the durable
source of truth for image chunks; BlobStorage becomes a
regional read-through cache + durable home for snapshot
chunks. Disk and memory canonical chunks live as OCI layers
(Nydus-shaped); chunk faults resolve `NVMe → BlobStorage →
OCI`, with opportunistic write-through fill. The cross-
namespace bricked-image failure mode (silent 404 on chunk
fault when bake-time and runtime BlobStorage namespaces
differ) is the headline thing this fixes.

Snapshot semantics, chunk sizes (16 MiB / 512 KB, disk-
aligned), and the canonical-memory MAP_PRIVATE primitive are
unchanged from ADR 0007. The architectural line is
canonical-vs-per-session, not disk-vs-memory: both canonical
disk and canonical memory live in OCI; both per-session
snapshot deltas (disk + memory) stay in BlobStorage.

The migration is gated on ADR 0007's stack landing in
production and validating real workloads. Rollout sub-
phases:

1. ⬜ **`ChunkResolver` trait introduced.** Default impl
   wraps current `BlobStorage` — no behavior change. Clean
   abstraction point for tiered fetch. Insertion crate:
   `engram-chunk-store`.
2. ⬜ **`OciChunkResolver` + Range GET in `engram-oci`.**
   Fault path falls through to OCI on BlobStorage miss.
   This phase alone fixes the cross-namespace bricked-image
   failure mode without any bake-side changes — useful even
   if later phases stall.
3. ⬜ **Bake produces Nydus-shaped artifacts.** Image-builder
   emits `bootstrap.disk + chunks.disk` (and memory
   counterparts) as OCI layers with new media types
   (`application/vnd.engram.bootstrap.{disk,memory}.v1+json`,
   `application/vnd.engram.chunks.{disk,memory}.v1`).
   `ImageBundle` schema bumps to v2. Scan + SBOM + referrer
   publish step (OCI 1.1 Referrers API) lands here.
4. ⬜ **Base/diff layer engineering.** Bake consults parent
   bootstrap at build time to skip chunks already in the
   base layer. Preserves cross-image dedup at OCI granularity.
5. ⬜ **Hybrid image_cache.** Conventional OCI and chunked-
   OCI artifacts coexist; format detected at pull time;
   both paths run. Long-term shape for environments that
   accept external/customer-supplied images.

Gating items / open questions outside this ADR's scope:

- **Per-registry Range-GET validation** (ECR / GAR / GHCR /
  Harbor known good; Docker Hub + self-hosted distribution
  vary). Gated in Phase 2.
- **Scanner integration choice** (Trivy assumed; pipeline
  is scanner-agnostic). Operator selects per deployment.
- **Referrer artifact lifecycle and GC per registry** — some
  registries don't evict referrers when the subject is
  deleted.
- **User-facing `[bake.warmup]` knob** for warm-daemon
  snapshots — separate ADR.
- **File-level chunking** (Nydus-default; stronger cross-
  image dedup but blocks VZ's materialize-to-file path) —
  separate ADR if/when justified.

See ADR 0008 for design rationale, the four-camp analysis,
the cost/benefit accounting, and the deferred decisions.

---

## Updating this doc

When you land a commit that touches anything above:

1. Move the matching ⬜ / 🟡 to ✅, cite the commit hash.
2. If the commit reveals a new gap, add it to the relevant phase.
3. If the punch list ordering needs to change (priorities shifted,
   new blockers found), rewrite it — but say *why* in the commit
   message.

Don't let this doc become aspirational. Either the work is in main
or it's pending.
