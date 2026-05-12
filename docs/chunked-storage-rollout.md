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

**Status: 🟡 partial — runtime + snapshot wiring shipped; bake-time
canonical capture deferred**

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

### Still pending

- ⬜ **Image-builder bake-time canonical capture** — boot VM during
  bake, pause, capture `memory.bin`, chunk + publish + record a
  canonical working-set trace. Until this lands every snapshot has
  `canonical_memory_manifest == memory_manifest`, so the
  cross-session page-cache sharing benefit isn't realised — restore
  still works, just at session-private memory cost.
  `crates/engram-image-builder/src/canonical_boot.rs` (new).
- ⬜ **Cross-host trace replay+publish wiring** — the coord-side
  cross-host migration slice will plumb `host_id` through to FC's
  restore so `--prefault-trace <host>` and `--publish-trace-host
  <host>` actually fire. Until then first-restore latency every
  time (correctness intact).
- ⬜ **Cross-host memory.bin materialization** — restore currently
  assumes the snapshot's `memory.bin` is still present on the host
  doing the restore (same-host hot resume). Cross-host migration
  needs `host-agent` to materialize memory.bin from the chunk
  manifest before invoking FC restore. Becomes meaningful only
  alongside the bake-time canonical slice (then the canonical mmap
  is the per-image shared file, not per-snapshot).

### Dependencies / open questions

- Kernel CONFIG_USERFAULTFD=y (Ubuntu cloud image — yes)
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

### Remaining (the full reshape)

The pieces below are the *destructive* / signature-changing
half. Splitting them off keeps the additive surface immediately
shippable + makes Phase 7 deletion cleanly excisable.

- ⬜ **Trait signature changes**:
  - `SandboxBackend::snapshot(id) -> SnapshotRef` (drop `dest: &Path`)
  - `SandboxBackend::restore(snap: SnapshotRef) -> SandboxId` (drop
    `src: PathBuf`)
  - `SandboxSpec.rootfs: ManifestRef` (drop `rootfs_source:
    Option<PathBuf>`); add `memory_canonical` + `working_set_trace`
- ⬜ **DB migration `0019_drop_cold_tier_columns.sql`** — drops
  `local_path`, `blob_present`, `replicated_at`, the
  envelope-encryption quartet. Ships alongside the
  `MetadataStore` method retirement (next bullet).
- ⬜ **`SnapshotRecord` reshape** — drop `local_path`,
  `blob_present`, `replicated_at`. Add `memory_manifest_*` +
  `working_set_trace_host` once Phase 5 produces them.
- ⬜ **`SnapshotResidency` enum deleted** — single tier now.
- ⬜ **`MetadataStore::flush_to_cold`, `clear_local_path`,
  `latest_cold_snapshot_for_session` deleted**.
- ⬜ **Coordinator `ensure_active` simplification** — three-branch
  (`Idle` hot, `ColdEvicted` cold, `Dead` 410) collapses to two
  (`Idle`, `Dead`).
- ⬜ **`engram-protocol::wire` `SandboxSpec` wire-shape bump** —
  the protocol surface for `RequestKind::CreateSandbox` carries
  the new `SandboxSpec` shape. `WIRE_VERSION` already exists
  (v2); this would bump to v3.

This is the single largest planned change. Touches ~10 files
substantially. Naturally pairs with Phase 7 deletion since the
trait-signature changes orphan the legacy code paths.

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
       `--mode=all` only today; multi-host fanout via WS-RPC is a
       follow-up (deliberate: avoid wire bloat until ops actually
       hit the leak in production). Cron scheduler pairs with #4.
4. ⬜ **Chunk-store GC scheduler** (Phase 1 gap) — coordinator cron
   loop + `POST /api/admin/gc-chunks` admin endpoint (the testable-
   trigger pattern per the feedback memory). ~50 lines.
5. ⬜ **Phase 6 trait reshape + migration 0018** — required for
   cross-host resume + spot preemption. Substantial: ~10 files.
6. ⬜ **Phase 7 tar.zst deletion** — clean up dead paths now that
   chunks are the source of truth.
7. ⬜ **Observability** — cache hit rate, chunk fetch latency,
   materialize time, GC counts. Without these, debugging production
   slowness is guesswork. Probably a Prometheus exporter; needs a
   framework call.
8. ⬜ **Phase 4 NBD** — FC restore time scales with image size
   without it. Could ship Tier 4 *without* this and accept ~16s
   restore for a 16 GiB image (materialize-first), then ship NBD as
   a perf upgrade.
9. ⬜ **Phase 5 UFFD + canonical memory + WS R&R** — the sub-100ms
   restore. Same "can ship without; eats latency budget" tradeoff.
10. ⬜ **Phase 8 Helm** — required for any K8s deploy.
11. ⬜ **Phase 9 Packer + Terraform** — required for self-serve
    provisioning of FC hosts.
12. ⬜ **Phase 10 ADR + docs** — alongside the deploy.

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

## Updating this doc

When you land a commit that touches anything above:

1. Move the matching ⬜ / 🟡 to ✅, cite the commit hash.
2. If the commit reveals a new gap, add it to the relevant phase.
3. If the punch list ordering needs to change (priorities shifted,
   new blockers found), rewrite it — but say *why* in the commit
   message.

Don't let this doc become aspirational. Either the work is in main
or it's pending.
