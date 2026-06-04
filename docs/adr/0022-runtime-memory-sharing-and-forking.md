# ADR 0022: Runtime guest-memory sharing & forking — File backend vs direct-mem

Status: 2026-06-03 — **Proposed (parked); Option A confirmed as the direction, Option B
rejected-for-now.** ADR 0021 took the substrate to FC-floor-bound (~516 ms warm / ~618 ms
cold) and made each enabled template's base memory **resident on NVMe**. This ADR scopes the
*next* lever it deliberately deferred: sharing that resident base memory **across live
sessions of the same template** (density) and the closely-related ability to **fork**
sessions. Still parked (no Option A implementation yet) — but [ADR 0028](0028-eviction-durability-under-host-roll.md)
shipped the diff-first checkpoint substrate and its dev-vm spike measured this ADR's
load-bearing assumptions, which **settles the Option-A-vs-B decision** (below) and builds
several of Option A's prerequisites. The remaining Option A work and the fork API are sized
in "Decision (updated 2026-06-03)".

### Decision (updated 2026-06-03, post-ADR-0028)

- **Lead with Option A; Option B is rejected for now.** ADR 0028's review eroded Option B's
  case from two directions: (1) **diff snapshots took its dirty-tracking pitch** — stock FC's
  KVM dirty-log + `SnapshotType::Diff` + the content-addressed chunk store deliver
  O(dirty-set) checkpoints with no FC fork (measured: **38 ms diff pause vs 2,090 ms full**,
  diff **3–4%** of guest RAM); (2) **checkpoint-anchored snapshot-fork took most of its fork
  pitch** — with 0028's continuous checkpoint chain, "fork session X" is a restore off X's
  latest (or an on-demand) checkpoint: zero-disturbance at ≤ one cadence interval staleness,
  or ~tens-of-ms parent pause at zero staleness. **Option B's only residual exclusive is
  zero-pause AND zero-staleness live fork simultaneously** — revisit only if that becomes a
  hard product requirement, and if so build/measure E2B's public `firecracker-v1.14-direct-mem`
  rather than patching from scratch.
- **Option A go/no-go: YES (measured).** The spike (`engram-sandbox-firecracker/tests/
  file_restore_shared_rss.rs`, now a permanent CI gate) restored **3 sandboxes File-backend
  off one `memory.bin`**: each RSS 87.8 MiB with **Shared_Clean 84.7 MiB**, **Σpss/Σrss = 35%**
  vs the perfect-3-way floor of 33% — plain page-cache sharing of one `MAP_PRIVATE` inode,
  no KSM. Restores 56–66 ms. Density holds on real hardware.
- **What ADR 0028 already built that Option A consumes:**
  - the **rolling per-session `memory.bin`** (`PooledBackend` checkpoint chain) — the
    fork/File-restore source is already materialized + maintained on NVMe;
  - **full-image chunk manifests per checkpoint** (`update_for_dirty_ranges`) — any checkpoint
    materializes to a contiguous memfile with no chain replay, which is exactly the
    File-backend `backend_path`;
  - the **`File` `RestoreMode`** + `track_dirty_pages` plumbing (`client.rs`,
    `FirecrackerConfig`);
  - **`events_cursor`** as the fork transcript cut-point and the **retention window** as the
    forkable-history window (ADR 0028 A.log + `checkpoint_retention`);
  - GC that already pins the latest checkpoint per live session — memfile-granularity pinning
    is the same pin-set extended.
- **Remaining Option A scope** (a follow-on arc, when density pressure is real): flip base
  `session.create` restores to `RestoreMode::File` against a **per-template** memfile
  materialized at prefetch (today each session restores its own); extend the chunk-GC pin set
  to that template memfile; measure prod shared-RSS + restore latency. Density only — no fork
  API in this arc.
- **Fork is its own later ADR.** The substrate is ready (checkpoint chain + cut-point + memfile),
  but a user-facing fork needs **session re-identity** — agentd bind / `session_env` re-stamp
  so the child wakes as a *different* session, per-child network identity, and the mid-run
  duplicate-external-side-effect semantics (the doubled version of ADR 0028 A.log's
  surviving-side-effect problem). That's a product-capability ADR, not infra.

---

#### Original framing (2026-05-29, retained for context)

It was parked — no implementation yet — to keep the decision (and its fork-vs-no-fork trade)
out of 0021 and let us pick it up measure-first. Nothing here is on the critical path for the
latency wins already shipped.

## Context

After ADR 0021, a session restore is dominated by Firecracker's own restore floor, and
the base template's working set is local (no GCS on the boot path). Two related wins are
left on the table, both about **runtime host RAM**, not latency:

1. **Density.** Our UFFD path serves faults with `UFFDIO_COPY`, which installs a
   **private** page per guest. So K live sessions of one template cost **K× the working
   set** in host RAM. If they instead shared the base's clean pages, K sessions would
   cost `base + K×dirty` — many more microVMs per host. This is explicitly attractive:
   packing density is a first-order goal, not just a latency tail-shave.
2. **Forking.** Spawning N sandboxes that start from one memory state and diverge (clone
   a warmed session / template) is a capability we'll want (cf. CodeSandbox's microVM
   cloning). It is the *same primitive* as density.

Both reduce to: **map an immutable, content-stable memory image into N guests
copy-on-write.** The question is *how*, and it forks cleanly into a no-FC-patch path and
an FC-patch path.

### The shared primitive (how memory COW works)

A guest restored from a memory file via `mmap(MAP_PRIVATE, fd)` reads clean pages straight
from the **host page cache**; the first write to any page triggers copy-on-write (the
kernel allocates a private anonymous page, copies the file page in, repoints the PTE). The
backing file is never mutated. So N guests mapping the same file share every clean page
from **one physical copy** and diverge privately on write. That is simultaneously the
density mechanism *and* `fork()` for guest RAM.

## Options

### Option A — Stock FC **File backend** (no fork)

Restore `session.create` via FC's `backend_type: File` against the **resident base
memfile** (materialized once per template per host from the resident chunks — aligned with
0021's residency). `MAP_PRIVATE` → page-cache sharing + per-session COW.

- ✅ **No FC fork.** Stock FC; we already run a compatible version.
- ✅ **Density**: same-template sessions share clean base pages via the page cache.
- ✅ **Substrate**: page-cache-warm restore, **zero UFFD round-trips** (no handler at all
  for the base path).
- ✅ **Forking**: `MAP_PRIVATE` *is* the snapshot-fork primitive (below).
- ✅ Aligns with 0021 residency (base memory is already local).
- ⚠️ Loses lazy-fault-from-GCS for the base — moot while residency keeps every base local.
- ⚠️ File and UFFD backends are mutually exclusive per restore. Plan: **File for
  `session.create` (base restore), UFFD/chunked for idle-resume** (unique per-session
  divergence, content-dedup at rest).
- ⚠️ Invariant: a shared memfile must be **immutable + pinned** for the lifetime of any
  sharer (extends the existing chunk-GC pin-set — ADR 0016 Phase C — to memfile granularity).

### Option B — `direct-mem`: memfd-backed guest RAM + `UFFDIO_CONTINUE` (FC patch)

Patch FC so guest RAM is backed by a shared **memfd** the handler owns; resolve faults
with `UFFDIO_CONTINUE` (MINOR mode) — zero-copy PTE installs against a shared, possibly
lazily-populated memfd.

- ✅ Everything File backend gives, **plus** lazy-fault-from-GCS *and* sharing
  simultaneously (matters only if we can't keep all bases resident), **online dirty-page
  tracking** (faster Pause / smaller diff-checkpoints), and **live, zero-pause fork** of a
  running VM.
- ❌ **Needs an FC fork.** Stock FC backs guest RAM anonymous-private + MISSING-mode even
  on latest (it uses memfd only with `vhost-user-blk`); there is no config knob. Verified:
  E2B carries this as a maintained **`firecracker-vX-direct-mem`** branch — a focused
  ~40-commit patch (`feat(memfd): allow using memfd to back guest memory`, + resident/
  dirty/zero-memory APIs, write-protection, and a virtio-blk DISCARD/WRITE_ZEROES block),
  **re-based per FC release** (v1.10 → v1.12 → v1.14). Public + Apache-2.0.
- ❌ Maintenance: own/track that patch per FC release; it's the security-sensitive VMM
  memory manager. Build-from-source pipeline (vs our release-tarball install).
- ❌ Higher correctness/security risk (patching guest-memory restore).
- ✅ Still a **jailed separate FC process** — no isolation regression (see below).

### Forking ladder (applies to both, with B extending it)

- **Template fork (no fork, no pause):** N sessions of one template `MAP_PRIVATE` the
  resident base memfile. Zero extra work — this is literally Option A density.
- **Snapshot fork (no FC fork):** fork a session = snapshot to a memfile, then
  `MAP_PRIVATE`-restore N children. One pause+write to materialize the fork point.
  **Idle-resume is the degenerate N=1 case** — divergence flattens into the snapshot at
  capture, restores on resume.
- **Live zero-pause fork (needs Option B):** children attach to a *running* parent's
  shared memfd with no checkpoint. The one capability that genuinely requires the patch.

### Ruled out — in-process VMM

Pulling FC's vCPU loop into the host-agent (a `git`-dep on FC's *unpublished* `vmm` crate,
or a from-scratch **rust-vmm** build) would reclaim the FC spawn + UFFD-socket handshake
(~100 ms) and give direct memory-backing control. **Rejected:** it dissolves the jailer
boundary (chroot + pid/net namespaces + dropped privileges + tight seccomp-bpf) that is
the whole point for untrusted code — *a guest escape must pass through both the guest
kernel and the jailer.* A VMM/escape bug would land in a high-privilege orchestration
process. FC stays a separate **jailed** process; the ~100 ms spawn/handshake is reclaimed
by **pre-spawning jailed FC**, not by going in-process. (E2B made the same call — patch the
memory backend, keep it jailed.) Note FC publishes no stable VMM library (only an
HTTP-client SDK on crates.io); only the lower-level *rust-vmm* crates (`vm-memory`,
`kvm-ioctls`, …) are reusable — so "use their crate" collapses into a fork either way.

## Comparison

| | **A — File backend** | **B — direct-mem (memfd)** |
|---|---|---|
| Density (shared base across VMs) | ✅ page cache | ✅ memfd |
| Substrate latency | ✅ no UFFD round-trips | ✅ zero-copy faults |
| Snapshot fork / idle-resume | ✅ | ✅ |
| Live zero-pause fork | ❌ | ✅ |
| Lazy-from-GCS **and** shared | ❌ (needs base resident) | ✅ |
| Online dirty-tracking / thin-disk | ➖ (basic diff snapshots are stock FC) | ✅ (bundled) |
| FC fork / build pipeline | **none** | yes, per-release patch |
| Correctness/security risk | low | high (VMM memory manager) |
| Jailer isolation | ✅ | ✅ |

**A is a strict subset of B's benefits at a fraction of the cost and risk.** The decision
hinges on two questions: (1) can we keep every base template resident on every host? (if
yes — and 0021's readiness gate does — A's only loss is moot); (2) do we need live-fork /
online dirty-tracking badly enough to own a fork?

## Decision (proposed, parked)

Lead with **Option A (File backend on the resident base memfile)** when this work starts —
it delivers density + the substrate win + snapshot-based forking with no fork, low risk,
and composes with 0021's residency. Treat **Option B (direct-mem)** as a deliberate later
escalation, justified only by hard data: a "too many templates to keep resident" wall, or
a concrete need for live zero-pause fork / online dirty-tracking. If we go there, build
and measure E2B's public `firecracker-v1.14-direct-mem` as the reference rather than
patching from scratch, and carry a trimmed memfd-only patch on upstream FC.

**First step when unparked:** a measure-first dev-vm spike of Option A — materialize a base
memfile from resident chunks, restore 2–3 sandboxes from it via the File backend, and
measure **shared RSS across siblings** (the density number) + restore latency vs today's
~600 ms. No production change until that number is in hand.

## Consequences

- Density (more microVMs/host) becomes achievable with **no FC fork** (Option A).
- Forking is unlocked as a future capability — snapshot-fork on stock FC, live-fork only
  if we later adopt Option B.
- A shared memfile is a new pinned, immutable artifact → extends the existing chunk-GC
  pin-set (ADR 0016 Phase C) to memfile granularity: a base/fork-point memfile is pinned
  while any child references it.
- Restore path bifurcates: File backend for base `session.create`, UFFD/chunked for
  idle-resume. Manageable, but a real branch in the FC backend.

## Open questions

1. Does File-backend page-cache sharing hold up at our density on real hardware
   (shared-RSS measurement, dev-vm)? KSM is not involved — it's plain page-cache sharing
   of one `MAP_PRIVATE` file.
2. Memfile lifecycle: materialization cost (full vs sparse file), and the pin/immutability
   invariant interplay with eviction + GC.
3. If/when Option B: trimmed memfd-only patch on upstream FC vs tracking E2B's full
   `direct-mem` branch; the per-release rebase burden; build-from-source pipeline.
4. Interaction with P3 warm snapshots (ADR 0021): a warm per-template snapshot is the
   ideal shared base memfile — these compose.

## References

- ADR 0021 (resident templates + warm snapshots) — the substrate this builds on; §4c/OQ1
  defer here.
- Firecracker `snapshot-support.md` (File backend `MAP_PRIVATE` page sharing),
  `handling-page-faults-on-snapshot-resume.md` (UFFD backend), `design.md` + `seccomp.md`
  (jailer isolation).
- `e2b-dev/firecracker` `firecracker-vX-direct-mem` branches + `e2b-dev/fc-versions`
  `build.sh` (proof E2B forks; the memfd patch shape).
- rust-vmm (`vm-memory`, `kvm-ioctls`) — the only reusable FC-adjacent crates.
- Linux `userfaultfd(2)` — `UFFDIO_COPY` (MISSING) vs `UFFDIO_CONTINUE` (MINOR).
