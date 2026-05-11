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

Phases 1–2 are shipped. Phase 3's read path is in (`--mode=all` only).
Phase 6's additive surface is in (`SnapshotMetadata.disk_manifest`)
plus VZ snapshot-time chunking. Phases 4, 5, and the full Phase 6
trait reshape, plus Phases 7–10, are pending. The standalone
host-agent binary has no blob / chunk-store / image-cache wiring —
that's the single largest production-deploy blocker.

---

## Phase 1 — Chunk store foundations

**Status: 🟡 partial**

### Shipped

- `4a87cd6` scaffold + manifest format + ChunkStore API
- `bace991` local NVMe cache with LRU, pinning, singleflight
- `e3b3699` file ↔ manifest helpers (`chunk_file`, `materialize_to_file`)
- `4510abb` `BlobStorage::list_prefix` + GC + manifest version lookup
- `d0b8126` GCS pagination + cross-session e2e tests against
  fake-gcs-server

### Remaining

- ⬜ **GC scheduler** — `ChunkStore::gc_unreferenced(retain_for)`
  exists at `crates/engram-chunk-store/src/gc.rs` but **nothing calls
  it**. Needs either:
  - A periodic task in `engram-coordinator::lib.rs::start_coordinator`
    (mirror of how `engram-host-agent::disk_pressure::spawn` was
    wired before retirement), or
  - An admin endpoint at `crates/engram-coordinator/src/api/admin.rs`
    so it's explicitly triggerable (matches the project's
    "explicit-trigger admin endpoints" preference)
  - Likely both — cron in prod, admin endpoint for tests.
- ⬜ **`list_prefix` on S3** — stub at
  `crates/engram-storage-s3/src/lib.rs` returns `Err(Config)`.
  Breaks GC on AWS. Fix when AWS lands (💤 for now).
- ⬜ **Local cache budget config** — `ChunkCache` takes a budget but
  there's no env / CLI flag plumbing it from the host-agent
  configuration. Today every consumer hard-codes a default.
- ⬜ **Schema-version-migration story** — manifests are
  `schema_version: 1`. No documented path for v2. One-line ADR note
  is enough for now; the writer rejects v≠1 already.

---

## Phase 2 — Image-builder chunked artifacts

**Status: 🟡 partial**

### Shipped

- `22efdc3` ext4 bake chunks the disk + writes a versioned manifest
  + drops a `bundle.json` sidecar with the manifest ref
- `1d8afbe` OCI push carries `bundle.json` as a third layer
  (`application/vnd.engram.bundle.v1+json`); host-agent's image_cache
  parses + surfaces it as `CachedImage.bundle`

### Remaining

- ⬜ **image-builder can't write to GCS today** —
  `crates/engram-image-builder/src/main.rs:139` hard-codes
  `LocalBlobStorage::new(images_dir.join("store"))`. **Production CI
  bakes can't produce usable artifacts**: chunks would land on the
  CI runner's local FS and be lost when the runner recycles. Needs:
  - `engram_image_builder::blob::from_env()` analogue of
    `engram_coordinator::blob::from_env` reading
    `ENGRAM_BLOB_BACKEND={local,gcs}` + `ENGRAM_GCS_BUCKET`
  - The chunk_root for the local path stays, but GCS path lands
    chunks directly in the deployment bucket
  - ~40 lines. **Production blocker.**
- ⬜ **OCI push still uploads the full `rootfs.ext4` layer** — this
  is wire-redundant once `bundle.json` is the source of truth. Phase 6
  retires it; until then every push transfers the disk twice (layer
  bytes + chunk bytes). For a 4 GiB rootfs that's wasted bandwidth +
  GCS spend.
- ⬜ **No memory canonical snapshot during bake** — the plan's Phase
  2 had image-builder boot the VM once and capture canonical memory.
  That's actually folded into Phase 5 in practice (UFFD work owns the
  canonical-base path).
- ⬜ **No working-set trace capture during bake** — same; Phase 5
  territory.
- ⬜ **`Format::Directory` bakes don't get chunked** —
  ProcessBackend is dev-only on macOS; acceptable not to chunk
  directories. Document this in the OCI bundle README so operators
  don't expect chunks on Directory bakes.

---

## Phase 3 — Disk adapter for macOS (VZ + Process)

**Status: 🟡 partial — slice 3a (read) shipped, slice 3b (write) shipped via Phase 6 additive**

### Shipped

- `d6a6281` host-agent `PooledBackend.create()` materializes
  `cached.bundle.disk_manifest` → content-addressed file under
  `<materialize_dir>/<manifest_id>-vN.ext4` → `spec.rootfs_source`.
  VZ's existing APFS-clonefile per-sandbox flow operates on top.
- `e68ee23` VZ's `snapshot()` chunks the cloned rootfs into the
  store and populates `SnapshotMetadata.disk_manifest`.

### Remaining

- ⬜ **Materialized file directory has no LRU / GC** —
  `<local_path>/chunked-rootfs/` grows unbounded over a host's
  lifetime. Two ways to close:
  - Swap `materialize_to_file` for `materialize_to_file_cached` (uses
    the existing `ChunkCache` for backing — LRU comes free) in
    `crates/engram-host-agent/src/pooled_backend.rs:materialize_chunked_rootfs`
  - Or add reachability-based cleanup co-located with
    `ImageCache::gc()`
- ⬜ **Standalone host-agent doesn't wire any of this** — see
  cross-cutting section below.
- 💤 **ProcessBackend chunked-rootfs path** — plan says "chunks
  become a cwd". Requires chunking Directory-format bakes. Low value
  (dev-only); deferred.

---

## Phase 4 — Disk adapter for Linux + FC (NBD daemon)

**Status: ⬜ pending — multi-session, needs dev-VM kernel validation**

The biggest remaining piece per the plan. Estimated ~800 lines.

### Scope

- `crates/engram-host-agent/src/disk_daemon.rs` (new) — NBD oldstyle
  / newstyle handshake + READ / WRITE / FLUSH dispatching against
  the chunk store
- `FirecrackerBackend::create` spawns the daemon, kernel attaches
  `/dev/nbdN`, FC's `path_on_host` points at that device
- Per-session dirty-region buffering, periodic flush, manifest
  version bumps on close
- Per-VM cleanup: `nbd-client -d /dev/nbdN` on destroy
- FC integration tests: extend
  `crates/engram-sandbox-firecracker/tests/exec_real_vm.rs` to assert
  the rootfs reads come through NBD

### Dependencies / open questions

- Kernel must have `CONFIG_BLK_DEV_NBD=y` (Ubuntu cloud image — yes;
  the Kata kernel we use for VZ — check)
- `nbd-client` userspace package on the host (Packer manifest needs
  it)
- Decision: kernel NBD client vs FUSE-backed block device vs vhost-
  user-blk. NBD has the simplest userspace story; vhost-user is the
  production-grade endgame.

### Until Phase 4 lands

FC sessions already work via the Phase 3a materialize-first path: the
chunked image is materialized to a file before VM boot, then attached
as `path_on_host` directly. Cost: ~16s materialize for a 16 GiB image
vs the NBD goal of sub-second. Functional, not fast.

---

## Phase 5 — Memory adapter (UFFD-from-chunks + canonical + WS R&R)

**Status: ⬜ pending — multi-session, the production magic**

Largest piece, ~1500 lines. Lights up sub-100ms restore + cross-VM
memory dedup.

### Scope

- Image-builder's bake step boots the VM once, pauses, captures
  `memory.bin`, chunks it (512 KB chunks), records a canonical
  working-set trace from a 5s post-restore warm-up
  - `crates/engram-image-builder/src/canonical_boot.rs` (new)
  - This is the work referenced as "deferred to Phase 5" under
    Phase 2 above
- `engram-uffd-handler` rewrite:
  - `crates/engram-uffd-handler/src/canonical.rs` — `mmap` canonical
    with `MAP_PRIVATE`; that FD goes to FC
  - `crates/engram-uffd-handler/src/working_set.rs` — record (first
    restore) + replay (subsequent restores)
  - `crates/engram-uffd-handler/src/chunk_resolver.rs` — page addr →
    chunk hash → UFFDIO_COPY
- `FirecrackerBackend::snapshot` + `restore` updated to thread
  memory manifests through

### Dependencies / open questions

- Kernel CONFIG_USERFAULTFD=y (Ubuntu cloud image — yes)
- Existing `engram-uffd-handler` crate is the host-side UFFD handler
  for FC today; Phase 5 is a substantial rewrite
- Cross-session canonical sharing relies on host page cache, which is
  fine on Linux but doesn't translate to macOS / VZ — VZ never gets
  memory chunking in v1 per the plan (its native memory-snapshot is
  upstream-broken anyway)

---

## Phase 6 — SandboxBackend trait + MetadataStore refactor

**Status: 🟡 partial — additive surface in, full reshape pending**

### Shipped (additive)

- `e68ee23` `SnapshotMetadata.disk_manifest: Option<ManifestRef>`,
  populated by VZ snapshot, threads through the trait without
  breaking signatures
- `ManifestRef` hoisted to `engram-core::types::manifest` to break
  dep cycle

### Remaining (the full reshape)

- ⬜ **Trait signature changes**:
  - `SandboxBackend::snapshot(id) -> SnapshotRef` (drop `dest: &Path`)
  - `SandboxBackend::restore(snap: SnapshotRef) -> SandboxId` (drop
    `src: PathBuf`)
  - `SandboxSpec.rootfs: ManifestRef` (drop `rootfs_source:
    Option<PathBuf>`); add `memory_canonical` + `working_set_trace`
- ⬜ **DB migration `0018_chunked_storage.sql`** —
  `deploy/migrations/`. Drops cold-tier columns, adds manifest refs.
  See plan for the SQL skeleton.
- ⬜ **`SnapshotRecord` reshape** — `disk_manifest_id`,
  `disk_manifest_version`, `memory_manifest_*`,
  `working_set_trace_host`. Delete `local_path`, `blob_present`,
  envelope-encryption quartet, `replicated_at`.
- ⬜ **`SnapshotResidency` enum deleted** — single tier now.
- ⬜ **`MetadataStore::flush_to_cold`, `clear_local_path`,
  `latest_cold_snapshot_for_session` deleted**.
- ⬜ **Coordinator `ensure_active` simplification** — three-branch
  (`Idle` hot, `ColdEvicted` cold, `Dead` 410) collapses to two
  (`Idle`, `Dead`).
- ⬜ **`engram-protocol::wire` changes** — `SandboxSpec` wire shape
  bump. Adds a real `WIRE_VERSION` constant or version-handshake at
  this point.

This is the single largest planned change. Touches ~10 files
substantially.

---

## Phase 7 — Delete v1 tar.zst path

**Status: ⬜ pending**

Pure deletion phase. Lands after Phase 6 since these files become
dead code only once the trait reshape removes their callers.

- ⬜ `crates/engram-host-agent/src/flush.rs` — delete
- ⬜ `crates/engram-host-agent/src/disk_pressure.rs` — delete
- ⬜ `crates/engram-coordinator/src/blob.rs` — delete the snapshot
  seal/unseal helpers (keep `engram-crypto::CredCipher` itself; it
  still serves `registry_credentials` + `session_secrets`)
- ⬜ `crates/engram-coordinator/src/api/admin.rs::flush_one`,
  `flush_idle` — delete
- ⬜ `start_coordinator` — drop the `disk_pressure::spawn` call
- ⬜ `local_path` references outside the cache layer — sweep + delete
- ⬜ `Cargo.toml` cleanup for crates that referenced the deleted code

---

## Phase 8 — Helm chart for coordinator

**Status: ⬜ pending**

`deploy/helm/engram-coordinator/`. Cloud-agnostic templates; cloud-
specific values in `values-gcp.yaml.example` / `values-aws.yaml.example`.

- ⬜ `Chart.yaml`, `values.yaml`, templates for `deployment`,
  `service`, `serviceaccount`, `configmap`, `secretproviderclass`,
  `hpa`, `pdb`, `networkpolicy`, `ingress`
- ⬜ Validation against a `kind` cluster (cloud-agnostic) + a real
  GKE cluster
- ⬜ README documenting the values shape

---

## Phase 9 — Packer + GCP Terraform

**Status: ⬜ pending — IaC only, presumes Rust gaps closed first**

`deploy/packer/` and `deploy/terraform/gcp/`. Packer image installs
firecracker + host-agent + chunk daemon + UFFD handler + systemd
units + drain hook. Terraform module provisions VPC, GKE, Cloud SQL,
Secret Manager, GCS bucket, Artifact Registry, MIG for FC hosts.

- ⬜ `deploy/packer/fc-host-gcp.pkr.hcl` + shared provisioner scripts
- ⬜ `deploy/terraform/gcp/modules/{network,gke,postgres,secrets,storage,artifact-registry,fc-host-mig,workload-identity}`
- ⬜ `deploy/terraform/gcp/examples/minimal/` — smallest viable deploy
- ⬜ `deploy/README.md` — infrastructure contract for other clouds
- 💤 `deploy/packer/fc-host-aws.pkr.hcl` + Terraform AWS module —
  contract documented, impl deferred

**Critical: this phase can NOT compensate for missing Rust wiring.**
Setting env vars in a Packer manifest only matters if the binary
reads them — see standalone host-agent gap below.

---

## Phase 10 — ADR 0007 + doc updates

**Status: ⬜ pending**

- ⬜ `docs/adr/0007-chunked-immutable-storage.md`
- ⬜ `docs/deploy.md` rewritten for chunked storage
- ⬜ `docs/known-issues.md` — retire entries closed by this work,
  add new ones for deferred work (AWS module, L2 cache, page-level
  memory dedup)
- ⬜ `DESIGN.md` — architecture diagrams, source-of-truth table,
  component descriptions
- ⬜ `README.md` — drop "two snapshot tiers" framing

---

## Cross-cutting: Standalone host-agent wiring

**Status: ⛔ blocked on design call for OCI credentials. Production blocker.**

The `engram-host-agent` binary (used in `--mode=host` multi-host
production) does not wire ImageCache, ChunkStore, or BlobStorage.
`crates/engram-host-agent/src/main.rs:222` constructs `HostAgent::new`
and `with_egress` only.

### What needs to land

- ⬜ `engram_host_agent::blob::from_env()` — mirror of
  `engram_coordinator::blob::from_env`. ~40 lines.
- ⬜ CLI flag / env in `crates/engram-host-agent/src/main.rs` for
  blob backend + bucket. ~10 lines.
- ⬜ `ChunkStore` construction + `HostAgent::with_chunk_store(...)`
  call. ~10 lines.
- ⛔ **`ImageCache` wiring** — needs an `OciClient`, which needs an
  `AuthResolver`. **Open design question**: how does a remote host-
  agent get registry credentials? Options:
  1. Anonymous resolver — only works for public registries
  2. Fetch creds from coordinator over the dialer WebSocket
     on-demand
  3. Wire host-agent directly to GCP Secret Manager (same Workload
     Identity binding the coordinator uses)
  4. Per-host credential file mounted by Packer (least flexible)

This blocks the standalone host-agent serving any session whose spec
carries an `image_uri` — i.e., every production session.

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

**Status: 🟡 one known break**

- 🟡 `SnapshotMetadata` gained `disk_manifest: Option<ManifestRef>`
  in `e68ee23`. Bincode is positional — coordinator and host-agent
  must ship at matching versions or deserialization mid-bincode
  payload misaligns.
  - Today the wire uses an `agent_version` string in the hello frame
    but **no formal protocol version**. The host-agent dialer
    doesn't currently refuse to connect on a mismatch.
  - Phase 6's wire-shape bump is the natural place to introduce a
    `WIRE_VERSION` constant + a strict hello-frame check. Until then,
    **a mixed-version deploy of this branch will misbehave** —
    rolling upgrades need to either drain or be wholesale.

---

## Production-deploy punch list (ordered by criticality)

Real-deploy blockers, in order:

1. **Image-builder GCS path** (Phase 2 remaining gap #5 above) — without
   this production bakes don't work at all. ~40 lines.
2. **Standalone host-agent wiring** (cross-cutting above) — without this
   `--mode=host` can't pull images. ChunkStore + BlobStorage parts are
   ~60 lines. The OCI credential question is a real design call.
3. **Wire-version enforcement** — without this, a mid-deploy version
   skew silently corrupts the protocol. ~30 lines + a hello-frame
   handshake.
4. **Materialized rootfs LRU / GC** (Phase 3 remaining gap) — long-
   running hosts exhaust disk. ~20 lines if we use
   `materialize_to_file_cached`.
5. **Chunk-store GC scheduler** (Phase 1 remaining gap) — storage
   bill creeps up forever. Coordinator cron + admin endpoint. ~50 lines.
6. **Basic observability** — cache hit rate, chunk fetch latency,
   materialize time. Without these, debugging production slowness is
   guesswork.
7. **Phase 4 NBD** — without this, FC restore time scales with image
   size (materialize-first). Functional but slow.
8. **Phase 5 UFFD** — without this, no sub-100ms restore. Functional
   but slow.
9. **Phase 6 trait + DB reshape** — required for cross-host resume +
   spot preemption.
10. **Phase 7 tar.zst deletion** — code-hygiene; no production
    behavior change.
11. **Phase 8 Helm** — required for any K8s deploy.
12. **Phase 9 Packer + Terraform** — required for self-serve provisioning.

Items 1–3 are the actual blockers for "engram on GCP, real users".
Items 4–6 are needed for a stable production. Items 7–12 are the
remaining surface from the plan.

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
