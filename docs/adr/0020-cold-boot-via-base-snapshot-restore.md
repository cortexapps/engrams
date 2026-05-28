# ADR 0020: Cold-boot via base-snapshot restore — 25 s → ~100 ms

Status: 2026-05-27 — **Proposed (P1 in progress).** ADR 0019 closed the
measurement phase and ranked **H1 (restore-from-base)** as the lever. This ADR
commits to the optimization: route every cold `POST /sessions` through the
existing, prod-validated `restore()` path against a per-image base snapshot,
then shave the ~1 s restore tail to ~100 ms. Phased P1–P5. **P1 is code-complete
and green** (9 commits `85f7223`..`c481e3c`, `just check` 822 tests + dev-vm
clippy). The dev-vm e2e was briefly blocked on a slow-cold-boot vsock handshake
issue (FC device-thread starvation) — **resolved by a capture-scoped handshake
retry, NOT io_uring** (see below; validating on the dev-vm). Flips to Accepted
only after the final phase is prod-measured.

Phase: 1 (P1 = the lever). Commit chain recorded here as work lands.

## The slow-cold-boot vsock handshake — root cause + resolution (not io_uring)

P1's code landed green, but the first dev-vm e2e (enable → `build_base_snapshot`
capture → create → restore) couldn't complete its agentd handshake.

**Root cause (kernel-stack-proven, 2026-05-27):** Firecracker runs *all* virtio
devices (block, net, **vsock**) on a single device thread, and with the default
**Sync block `io_engine`** that thread does blocking `read()`s. The capture VM's
rootfs is `/dev/nbd0` (chunked-NBD), so each read goes kernel-NBD → the engram
userspace daemon → chunk cache/blob (slow). Sampling the FC process mid-boot
showed the device thread in **D-state** with stack `folio_wait_bit_common →
filemap_read → blkdev_read_iter → vfs_read → read()`, syscall = `read` on
fd → `/dev/nbd0`, 4 KiB. While blocked there it can't service the vsock queue →
the guest agentd's `connect()` to the host ready port (1027) goes unanswered →
times out at the kernel's ~9 s vsock connect timeout → the handshake fails. The
whole capture boot is ~106 s for the same reason. (Prod survives because its
rootfs reads are fast — warm NVMe; even cold it reads at 16 MiB chunk
granularity over GCS, ~84 ms × ~162, not 4 KiB × ~90k — so the device thread
never blocks past the 9 s timeout.)

**Why not io_uring (decided 2026-05-27 after an investigation pass):**
- Engram adopting io_uring for its *own* I/O doesn't pay: GCS is network-bound
  (io_uring saves µs against ~84 ms RTTs); chunk-cache/NBD reads are already
  async at 16 MiB granularity → bandwidth-bound, io_uring's per-op win is noise;
  the UFFD bottleneck is the single-threaded fault loop, not syscalls. And the
  162-sequential-fetch is a *concurrency* problem (the guest's ext4 mount demands
  chunks one-at-a-time) — addressed by working-set prefetch + restore-from-
  snapshot, which io_uring wouldn't touch.
- FC's *global* Async (io_uring) block engine **does** fix the starvation, but
  it's FC-labeled developer-preview (not for production) and — critically — its
  ~110 ms device-creation cost likely bakes into `state.bin` (restore never
  re-issues `put_drive`; `io_engine` is immutable post-load, un-`patch_drive`-
  able), so it would re-pay on **every restore** — taxing the exact path we're
  driving to ~100 ms. Hypothesis, but enough to avoid flipping it globally.

**Resolution — capture-scoped handshake robustness, restore stays on Sync.**
The starvation only bites the one-time *capture* cold boot (and any slow cold
boot); the restore path has no ext4 page-in (memory via UFFD) and pre-sets
`agent_ready`, so it's untouched. The handshake is made to wait out the
slow-read window instead of giving up at 9 s:
- **agentd retries** the ready-port dial in a bounded loop
  (`crates/engram-agentd/src/main.rs`) — once its own pages are resident and the
  read storm subsides, a re-dial connects. On a fast boot the first dial wins, so
  it costs nothing; restored sandboxes never re-run this path.
- The **host ready listener loops** (`spawn_agent_ready_listener` in
  `crates/engram-sandbox-firecracker/src/lib.rs`) — re-accepts across failed
  reads and keeps the `agent_ready` watch sender alive, so `wait_agent_ready`
  waits its full 180 s instead of returning "channel closed" on the first miss.

(Rejected: bumping `/dev/nbd0` readahead to coalesce the 4 KiB reads — it only
makes boot fast enough to *mask* the starvation; fold it in later as a perf win,
not the fix. No ADR 0021 / io_uring effort is being pursued.)

**P1 happy-path validated end-to-end on the dev-vm (2026-05-27):** enable
`localhost:5001/demo:warm-3` → `build_base_snapshot` capture (HTTP 201, base
snapshot recorded) → `POST /sessions` → `kind:"restored"` Active in ~13 s →
`exec` in the restored guest returns `exit 0` (`Linux … Debian 12`,
`restored-and-alive`). So capture (1a/1b), transactional enable, and
create→restore with option-D harness handling (1c) all work on real FC.

**Measured restore breakdown (dev-vm coord log, the ~13 s):** memory.bin
materialization from 237 chunks ≈ **6.4 s** (rebuilding the full 4 GiB file);
File-mode `load_snapshot` eager read ≈ **0.64 s** (fast — memory.bin was just
written, page-cached); agent handshake 15 ms. So the dominant cost is **rebuilding
a 4 GiB memory.bin from chunks** (`materialize_memory_if_missing`, which runs
*unconditionally* in `PooledBackend::restore`), NOT the eager load. (Earlier
numbers in this ADR speculated "~1 s prod / cold-blob" — that was wrong and
conflated two paths: ADR 0019's measured ~1 s is *warm same-host idle→resume*,
where memory.bin is already on local disk and there's no materialize. A
*base-snapshot* restore is cross-host/first-touch and pays the materialize
everywhere, prod included. The dev-vm disk is the boot PD, not a separate local
NVMe, but the chunks are local — the cost is the reconstruction, not blob
coldness.)

What still remains in P1: **1d** per-fork identity reseed and **1e** per-snapshot
restore serialization (both concurrency-only — they gate the prod push, not the
happy-path validation), then prod measurement.

The restore-latency win (eliminating the ~6.4 s materialize) is the **UFFD
milestone — see below; deliberately scoped as its own effort, not P1/P2 inline.**

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

## CPU-family portability — capture on a prod host, pin T2CL

Firecracker memory snapshots embed the capture host's CPUID/MSR state. Built-in
FC CPU templates are **vendor-pinned** (`T2CL`/`T2`/`T2S`/`C3` Intel-only, `T2A`
AMD-only); none bridge AMD↔Intel. Our CI bake runs on Blacksmith's **AMD** pool;
prod FC hosts are **Intel Cascade Lake** (`n2-standard-8`, `min_cpu_platform =
Intel Cascade Lake`). An AMD-baked snapshot restored on Intel already broke prod
once (2026-05-21: glibc ifunc picked AMD-only AVX-512 → every `fork+exec`
segfaulted at `RIP=0`). This is exactly why ADR 0015 M5 dropped snapshots from
the bake artifact — and why ADR 0020, which reintroduces them, must solve it.

**Decision: capture the base snapshot on a prod host, not in the CI bake.** The
image-builder stays CPU-agnostic (docker build → ext4 → chunk). The FC capture
runs on a real prod Intel host with `ENGRAM_FC_CPU_TEMPLATE=T2CL` set
(`cpu_template_from_env`, already plumbed into `FirecrackerConfig`). Capturing on
an Intel host is what lets us *use* `T2CL` at all (it failed on AMD), and T2CL
masks to the Cascade-Lake baseline so the snapshot restores cleanly on every
Intel-CL-or-newer host — `min_cpu_platform` is a floor, not a ceiling, so GCE may
place a session on newer silicon; the template makes that irrelevant.

**Enable is transactional over the capture (no partial state).** An image is not
enabled unless its base snapshot was captured and recorded. The capture (a host
RPC, ~15–30 s) runs *outside* any DB transaction; on success the coord opens one
short PG transaction writing the `snapshots` + `base_snapshots` + `enabled_images`
rows atomically. Capture failure ⇒ zero rows ⇒ image stays un-enabled, surfaced
to the operator. This removes the "enabled but unsnapshottable" state entirely,
so the create path can treat a base-snapshot hit as the norm (cold create stays
as a defensive fallback for legacy / pre-0020 rows).

## Phase 1 — Route cold create through base-snapshot restore [15 s → ~1 s]

The single highest-leverage step: it removes the kernel boot, both ext4 mounts,
and 100 % of the 13.6 s serial page-in in one move.

- **1a. Host-side base-snapshot capture** (`crates/engram-host-agent/`,
  `crates/engram-sandbox-firecracker/`). A new host operation
  `build_base_snapshot(image_uri, manifest_digest, vcpus, memory_mib)` composes
  the existing `create` + `snapshot` machinery: materialize the rootfs (NBD from
  the chunk store), boot a microVM under FC with the **stub harness** attached
  and `cpu_template = T2CL` (option-D capture point — bootstrap on `accept()`,
  harness *unmounted*) → pause → full FC snapshot (`SnapshotType::Full`) → chunk
  `memory.bin` into the chunk store → upload `state.bin` + sidecar JSON to
  `snapshots/<id>/{state.bin,sidecar.json}` → destroy the capture VM → return the
  `SnapshotMetadata`. Reuses `FirecrackerBackend::snapshot` + `PooledBackend`'s
  chunk/upload path; the only net-new is the create→pause→snapshot→destroy
  orchestration with no session attached. Exposed coord→host over gRPC.
- **1b. `base_snapshots` table + transactional enable.** Migration
  `deploy/migrations/0031_base_snapshots.sql`:
  `base_snapshots(manifest_digest TEXT PK, snapshot_id UUID → snapshots(id),
  image_repo, image_tag, vcpus, memory_mib, created_at)`. In
  `crates/engram-coordinator/src/api/enabled_images.rs`: `enable_image` (and
  `refresh_enabled_image`) pull/validate the manifest + materialize disk chunks
  (existing), then call `build_base_snapshot` on a host **and block**; on success,
  in one PG transaction, record the `snapshots` row (`session_id=NULL`, nullable
  since `0028`; `recoverable=true` after HEAD-verify) + the `base_snapshots` row +
  the `enabled_images` row. Capture failure aborts the whole enable. Memory chunks
  are in BlobStorage from the capture, so first session is a GCS pull (intra-VPC),
  not an OCI pull.
- **1c. Branch the create path into restore** (`api/sessions.rs`,
  `create_session_inner` ~626–674). Look up `base_snapshots` by
  `enabled.manifest_digest`. On hit: build a `SnapshotMetadata` like
  `resume_from_fc_snapshot`, set `prefer_snapshot_id`, call `restore_for_session`
  instead of `create_for_session`, then reuse `finish_resume_to_active` for
  post-restore identity binding + egress + `start_agent`'s `SpawnHarness` RPC
  (per-session harness late-bound via `swap_harness_drive`/`patch_drive`). On
  miss/error: fall through to the existing cold `create_for_session` with one
  `tracing::warn` — mandatory fallback for older images.
- **1c′. Per-session env injection (restore-model consequence).** A base
  snapshot is shared across all sessions of an image, so it can carry only the
  *generic* manifest env — not per-session literal secrets or `ENGRAM_SESSION_*`.
  In cold-create those rode `vm_spec.env` into the sandbox; on restore they must
  be injected post-restore. `restore_base_for_session` takes the session's
  `spec_env` and the host merges it into the restored sandbox
  (`SandboxBackend::merge_session_env`). ProcessBackend merges into its spec env
  (read by `exec`). **FC follow-up:** the FC guest is already running from the
  snapshot, so its env can't be rewritten host-side — the harness gets session
  env via `start_agent`'s `AgentSpec.env`; sandbox-wide exec-env injection on FC
  restore needs an agentd-side merge (tracked separately, not P1-blocking since
  the harness — the secret consumer — is covered).
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

- **DEDICATED MILESTONE — chunk-native UFFD ("Route B"), decided 2026-05-27.**
  The biggest restore win, carved out as its own effort. **Discovery:** the
  binaries run the **default `RestoreMode::File`** — neither `engram-coordinator`
  nor `engram-host-agent` main sets `restore_mode`, nothing overrides it, and
  `engram-uffd-handler` isn't deployed by packer. So the chunked-memory UFFD path
  (load_snapshot_uffd, spawn_uffd_handler, canonical mmap, prefault traces — all
  implemented + tested) is **dead in production**; every restore rebuilds the full
  memory.bin and eager-loads it.

  **Corrected cost model (2026-05-27).** Earlier notes overstated this as "copy
  4 GiB three times." `materialize_to_file_cached` writes a **sparse** file
  (`set_len` + seek-past-zeros, `chunk-store/src/file.rs:160`) and the captured
  base manifest is only **237 × 512 KiB ≈ 118 MiB** of non-zero chunks (a freshly-
  booted idle Debian — most of the 4 GiB is zero). So prefetch + materialize move
  ~118 MiB, not 4 GiB; File-mode `load_snapshot` was ~0.64 s only because the zero
  regions are sparse holes that fault to zero pages without disk reads. The dev-vm
  ~13 s is ~118 MiB + 237 small-file ops on a slow boot PD, amplified.

  **Why current UFFD also wins nothing.** `runtime.rs:~287` mmaps the materialized
  `memory.bin` with `MAP_PRIVATE | MAP_POPULATE` and serves canonical-resolved
  faults from that mmap — so even in UFFD mode we'd still materialize **and**
  eager-fault the whole file. For a *base* snapshot, `canonical_ref ==
  session_ref` so *every* fault resolves `Canonical` → the chunk-fetch path is
  dead. Flipping `restore_mode = Uffd` as-is = File cost + an extra process.

  **Decision — Route B (chunk-native): drop `memory.bin` entirely.** The handler
  serves **every** fault from the chunk cache/store (`fetch_chunk` + `UFFDIO_COPY`)
  and zero-filled chunks via `UFFDIO_ZEROPAGE`; there is no canonical mmap and no
  materialize step. FC resumes immediately and the ~118 MiB working set streams
  from the warm local cache (optionally prefaulted from a working-set trace). This
  is **strictly cheaper than today's File mode** and the simplest end-state — it
  matches the "no `memory.bin` file to ship" portability claim already in the
  `chunked.rs` docstring.

  **Why not "Route A" (keep memory.bin, just drop MAP_POPULATE).** Considered and
  rejected. Its only claimed advantage was cross-session memory dedup — but
  `UFFDIO_COPY` (used by *both* routes) always installs a **private** page, so
  guest RAM is never shared in either route. Route A's "dedup" is only the *source*
  `memory.bin` page cache being shared across handlers, which Route B gets for free
  via the shared chunk-cache files. So Route A trades away nothing real and keeps
  the materialize pass. (The `chunked.rs` docstring's "1000 sessions cost 4 GiB +
  deltas" claim is aspirational — it refers to source-side page-cache sharing, not
  guest-RAM dedup, which neither route delivers.)

  **FUTURE IMPROVEMENT — true packing via `UFFDIO_CONTINUE` (not in this
  milestone).** The *only* way to get both fast lazy boot **and** real guest-RAM
  dedup is minor-fault handling over a **shared per-image backing** (memfd/tmpfs or
  hugetlbfs): register guest RAM UFFD in **MINOR** mode, maintain one shared
  canonical backing per image (populated lazily from chunks — first faulter fills
  a shared page), and serve canonical faults with `UFFDIO_CONTINUE` (installs a PTE
  pointing at the *already-present shared page* — no copy, COW on write); divergent
  writes still `UFFDIO_COPY` to private pages. This is how dense snapshot-restore
  systems pack many same-image sandboxes per host. **Why deferred, not built now:**
  (1) stock Firecracker (we run v1.10.1) backs guest RAM as anonymous-private and
  registers MISSING-mode — `CONTINUE` needs FC to back guest RAM with a shared
  memfd, which must be **verified to exist in our FC build** before promising it;
  (2) host kernel needs the shmem/hugetlbfs minor-fault feature (5.13/5.14+);
  (3) the handler becomes **stateful per-image** (shared-backing lifecycle,
  refcounting, eviction) — more moving parts on a reliability-critical path. Route
  B is the clean substrate for this upgrade: the per-page resolution logic is
  identical, only the install primitive (`UFFDIO_COPY` → `UFFDIO_CONTINUE`) and the
  backing change. Gate the upgrade on (a) measured host-density pressure and
  (b) confirmed FC minor-fault support — don't build the stateful path up front.

  **Route B scope:** (1) `fc_cfg.restore_mode = Uffd` from an env knob in both
  mains; (2) build + deploy `engram-uffd-handler` (packer provisioner) +
  `/dev/userfaultfd` perms, co-located with `firecracker` on the FC host (it
  receives the UFFD fd via SCM_RIGHTS over a same-host UDS); (3) rewrite the
  handler's canonical-serve path to fetch chunks + `UFFDIO_ZEROPAGE` instead of the
  `memory.bin` mmap, and drop the `--canonical-memory` arg; (4) make
  `PooledBackend::restore` **skip** `materialize_memory_if_missing` for UFFD (keep
  the prefetch — it warms the cache the handler faults against); (5) the
  MAP_POPULATE drop is subsumed (no mmap at all); prefault-trace work in 2b below
  still applies. Measure before/after via the Jaeger spans (wire
  `OTEL_EXPORTER_OTLP_ENDPOINT` → the dev Jaeger; the ad-hoc dev-vm coord launch
  didn't, which is why the ~13 s breakdown above came from the coord log).
- **Route B VALIDATED on the dev-vm (2026-05-27).** Built `engram-uffd-handler`
  with the chunk-native rewrite, relaunched mode=all coord with
  `ENGRAM_FC_RESTORE_MODE=uffd` + `ENGRAM_FC_UFFD_HANDLER_BIN` + OTLP→Jaeger, and
  restored the existing `demo:warm-3` base snapshot. End-to-end: `skipping
  memory.bin materialization` → handler handshake (2 regions, 4 GiB) → `firecracker
  microVM restored from snapshot mode=Uffd` → fault loop serves chunks on demand
  (1→64) → session `active`, `kind:"restored"` → `exec` returns exit 0 (`Linux
  5.10.223`, Debian 12.14). FC `resume vm` took **159 µs** (vs File mode's
  multi-second eager read). Two non-bugs found en route: (a) the handler panicked
  "no reactor running" because `engram_telemetry::init` (OTLP batch exporter spawns
  a task) ran *before* the tokio runtime was built — fixed by moving init + the
  listener inside `rt.block_on`; (b) a one-off `HostLost` that was just a stale
  in-proc host entry after a ~9-min idle debugging gap (resolve_owner's
  `entry_is_fresh` TTL), not a restore bug — reproduced green on a fresh coord with
  an immediate create. Remaining before prod: packer deploy of the handler binary +
  `/dev/userfaultfd` perms; prod measurement.
- **Route B SHIPPED + PROFILED on prod (2026-05-27).** Deployed end-to-end: OSS
  push → bake-images `detect` (now a cargo-dep-graph lane detector, replacing a
  drifted path denylist) → `engrams-host-changed` → FC-host bake (host-agent +
  uffd-handler musl; needed `linux-libc-dev` + `CFLAGS_*` so `userfaultfd-sys`'
  C shim finds `<linux/types.h>`) → `fc-host-baked` → tf-apply → MIG rolled to a
  handler-baked image with `ENGRAM_FC_RESTORE_MODE=uffd` (now the `fc-host-mig`
  TF default; `vm.unprivileged_userfaultfd=1` baked by packer). Host
  `engrams-fc-rq3b` confirmed `restored from snapshot mode=Uffd`, agent handshake
  68 ms.

  **Prod Claude Code session profile (bogus key → Anthropic 401), cold (1st on a
  freshly-rolled host) vs warm (2nd):**

  | phase | cold | warm |
  |---|---|---|
  | enable image = base-snapshot capture (one-time) | ~70 s | — |
  | POST → Active (UFFD restore + harness ext4 bind) | ~11 s | ~4.5 s |
  | Active → Claude Code `run_started` (node/JS startup) | 7.2 s | 9.1 s |
  | `run_started` → "Invalid API key" (Anthropic 401 RTT) | 0.22 s | 0.24 s |
  | end-to-end (POST → auth error) | ~18 s | ~13.9 s |

  **Key finding — the bottleneck moved off everything UFFD touches.** UFFD restore
  is cheap (~3–4 s cold, faster warm) and the Anthropic round-trip is negligible
  (~0.2 s). The two dominant terms are the **harness ext4 bind** (warms well:
  11 s→4.5 s as chunks + the harness pack cache locally) and **Claude Code's own
  node/JS cold startup (~7–9 s), which does NOT warm** (CPU-bound interpreter boot
  inside the guest; it drifted *up* on the warm run). Prod NVMe also confirmed the
  dev-vm's ~104 s node startup was slow-PD pathology, not a real cost.

  **Implication for the roadmap (supersedes P2's framing as the next lever).** The
  base snapshot is captured with a **stub harness** (option-D), so the restored VM
  has no agent process — every session cold-starts node. The highest-leverage next
  step is a **per-harness "warm" snapshot**: capture *after* the harness (e.g.
  Claude Code) has booted to idle, so restore lands in a ready-node state and the
  ~7–9 s vanishes. This is a larger milestone than P2/P3 but is squarely where the
  prod data points. P2's memory working-set prefetch is now low-value (memory
  restore is already cheap); the working-set idea is better spent on the **harness
  substrate + node/JS pages**.

  **Telemetry gap (blocks finer profiling).** Cloud Trace had **0 traces in 2 h** —
  the coord pod has no otelcol collector sidecar (it exports OTLP to `localhost:4317`
  with nothing listening; spans silently dropped), the residue of the 2026-05-27
  collector-sidecar incident (see [[telemetry_must_not_gate_workload]]). The
  cold/warm figures above came from host journald + the conversation log; the
  finer restore sub-span split (`prefetch` vs `load_snapshot` vs fault-serving, +
  the handler `uffd.run` spans) needs the collector restored as a native sidecar.
- **2a.** ~~`runtime.rs:~288`: `MAP_PRIVATE | MAP_POPULATE` → `MAP_PRIVATE` +
  targeted `MADV_WILLNEED`.~~ **Subsumed by Route B** — the chunk-native handler
  has no canonical mmap at all, so there is no `MAP_POPULATE` to drop. Lazy
  fault-in is inherent; the `uffd.map_populate` span is removed with the mmap.
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
