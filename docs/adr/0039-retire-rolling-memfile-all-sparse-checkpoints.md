# ADR 0039: Retire the rolling memfile — all-sparse periodic checkpoints

Status: 2026-06-05 — **Accepted.** Implemented (commit chain below) and
validated on the Linux dev-vm with real microVMs:

- **All-sparse end-to-end** (`checkpoint_chain`, real FC, 60.45 s): Full
  seed (v1) → **Diff (v2, same manifest_id)** — proving the *fresh*-seeded
  chain now DIFFS via the sparse path (the leg that used the rolling
  memfile before this ADR) → restore-mid-chain (markers byte-identical) →
  post-resume sparse Diff (v3, same id) → restore (all three markers
  byte-identical). No Full re-seed, no 60 s hang.
- **Leak-fix guard:** the test asserts a Full capture leaves NO local
  `memory.bin` in the snapshot dir, and the restore still succeeds
  (re-materialized from the chunk manifest). The macOS-runnable unit
  `restore_materializes_missing_state_and_sidecar_from_blob_storage`
  asserts the same, so the guard runs in CI without the FC gate.
- `just check` green (976 unit tests; fmt + clippy + check) on macOS;
  `cargo clippy -p engram-host-agent --all-targets` clean on the dev-vm
  (the Linux-gated integration test).

Prod follow-ups (post-merge, after the FC-host MIG re-bake auto-rolls):
new `checkpoints/` (rolling) and leaked `snapshots/**/memory.bin` growth
both stop; a one-shot prod-ops cleanup reclaims the existing 61G (this
code only prevents *new* growth). The `snapshots/` dir reaper + the
chunk-cache LRU remain the queued follow-ups.

Continuation of **ADR 0038** (low-pause periodic checkpoints). 0038
introduced the sparse re-chunk primitive
(`update_for_dirty_ranges_sparse`) and used it for the *resume*-seeded
chain — but left the **rolling memfile** in place for chains seeded by a
fresh **Full** capture (`CheckpointChain.rolling_memfile: Some(path)`,
the File-mode overlay path). This ADR retires the rolling memfile
entirely: **every** checkpoint chain is sparse.

## Context

Prod FC hosts ran to **100% disk** within hours of serving sessions.
`engrams-fc-vzkr` breakdown:

- **61 G `snapshots/`** — seven committed Full snapshots × an **8 GiB
  `memory.bin`** each, plus one partial. The Full capture chunks the
  dump into the content store but **never removes the local
  `memory.bin`**, and `commit_snapshot` keeps all artifacts. Worse,
  `seed_checkpoint_chain` then **copies** that 8 GiB `memory.bin` into
  the rolling slot — a second resident full image per fresh-seeded
  session.
- **8.1 G `checkpoints/`** — the rolling memfiles themselves (one full
  guest-RAM image per live fresh-seeded session, the diff-overlay
  target).
- 21 G `chunk-cache/` — out of scope here (LRU is a separate follow-up).

The rolling memfile exists to serve the diff-capture re-chunk: overlay
the sparse `memory.diff` onto the resident full image, then re-chunk the
touched chunks (`overlay_sparse` + `update_for_dirty_ranges`). It is a
**capture-side artifact only** — it is the diff-apply *target*, never
read on the resume/restore path, and it is removed when the sandbox is
destroyed. Its sole benefit over the 0038 sparse path is during the
periodic-checkpoint re-chunk (which, post-0038's B3, runs **after
resume, off the frozen-guest pause**):

- **Warm chunk cache (the common case):** ~no benefit. The sparse path's
  `get_chunk(prev_hash)` hits the NVMe cache — the prev chunks were
  faulted in by UFFD precisely *because* the guest wrote them this
  interval — so both paths are O(dirty set) local I/O and complete in
  the same ballpark.
- **Cold/evicted cache (tail):** the rolling memfile avoids a GCS fetch
  per cold prev chunk (~87 ms each in prod). This is a capture-tail /
  `capture_lock`-hold robustness benefit, never a guest-pause or resume
  benefit.

That tail-robustness is not worth **8 GiB resident disk per session**
when the fleet is disk-bound. The sparse path (validated in 0038) gets
the same diff for free off the warm cache.

## Decision

**All checkpoint chains are sparse. The rolling memfile is removed.**

1. **Seed sparse, always.** Fold the fresh-Full seed
   (`seed_checkpoint_chain`) into the resume seed
   (`seed_checkpoint_chain_sparse`): insert the chain manifest-only
   (`rolling_memfile` gone), with **no `std::fs::copy` of the 8 GiB
   `memory.bin`**.
2. **Remove `memory.bin` after chunking.** Once a Full capture has
   chunked the dump into the content store, the local `memory.bin` is
   redundant (the chunks are durable; the sparse re-chunk fetches prev
   from the cache; restore re-materializes from chunks). Remove it in
   the post-capture block — mirroring how the diff branch already
   discards `memory.diff`. This is the root-cause fix for the 61 G leak.
3. **Diff branch is always sparse.** Drop the `rolling_memfile`
   `Some/None` dispatch; every diff goes through
   `update_for_dirty_ranges_sparse`.
4. **Retire the rolling machinery.** Remove the
   `CheckpointChain.rolling_memfile` field, `checkpoint_rolling_path`,
   the `overlay_sparse` call site / `update_for_dirty_ranges` (File-mode
   overlay) usage, and the `destroy`-path rolling cleanup. Net: this
   *removes* code (per the simplify-via-abstractions ethos).

Eliminates both big disk consumers: the 8.1 G rolling (gone) and the
61 G leaked `memory.bin`s (removed after chunk).

## Feature-flag decisions (restore mode)

The prod-latency investigation (session `0e22a4e9`) settled the
FILE-vs-UFFD question and the lingering code-default ≠ prod-default
mismatch. There are two restore workloads, two flags
(`effective_restore_mode(fresh)` bifurcates):

- **`ENGRAM_FC_RESTORE_MODE` (idle→active resume) default flipped
  `file → uffd`.** UFFD lazy-fault is the one true resume path: a
  cross-host idle-resume can't rely on a resident image, so File there is
  a synchronous multi-GB reconstruct. Prod already runs uffd via the OSS
  `fc-host-mig` TF module (`var.restore_mode` default) — which **this PR
  removes** so the code default flows through. `file` stays the explicit
  opt-out (no chunk store / no `/dev/userfaultfd`).
- **`ENGRAM_FC_BASE_RESTORE_MODE` (base `session.create` / warm-pool)
  default flipped `inherit(None) → file`.** Base templates are locally
  resident, so File restore is a fast local read (ADR 0022 Option A
  density). This **promotes ADR 0022's Option A from canary to default** —
  ADR 0022 should flip to Accepted on the back of this.

**Activation + caveat (deploy coupling):** the restore-mode override lived
in the OSS **`fc-host-mig` TF module** (`var.restore_mode` default `uffd`,
hard-set into the host systemd env) — **not** Helm; and
`ENGRAM_FC_BASE_RESTORE_MODE` was never set anywhere (base was already
code-default-driven). **This PR removes the module override** (the env line
+ the now-unused `var.restore_mode`), so on merge both resume and base flow
from the code defaults via the FC-host re-bake + MIG roll. Resume is
unchanged (`uffd → uffd`); **base flips `uffd → file`** on that roll — so
verify the per-template **resident base memfile** is present on rolled hosts
(a cold host without it falls back to materialize-from-chunks, the serial
path) and watch cold-boot latency. The struct default (`FirecrackerConfig`)
stays `File`/`None` for test safety; only the env-parser defaults moved.

**Dead-path note (further cleanup):** with the env path always resolving a
concrete base mode, the `None`-inherit branch in `effective_restore_mode`
is reachable only via direct struct construction (tests). A later pass can
drop the `Option` / inherit semantics.

## Alternatives considered (and rejected)

- **Keep the rolling memfile + rename-not-copy the seed + add a
  `snapshots/` reaper.** Was the first plan (move `memory.bin` → rolling
  instead of copying, halving the leak). Bounds the leak but keeps 8 GiB
  resident per session for a capture-tail benefit that doesn't justify
  the disk on a disk-bound fleet. All-sparse is strictly less disk and
  less code.
- **Chunk-cache LRU now.** Orthogonal — addresses the 21 G cache, not the
  snapshot/checkpoint growth this ADR targets. Deferred.

## Invariants / correctness

- **Sparse re-chunk correctness is unchanged from 0038:**
  `prev_chunk ⊕ dirty_pages == current guest memory`, KVM dirty tracking
  re-armed at `load_snapshot`. Retiring the rolling path only removes the
  *other* (overlay) way of computing the same chunk set — already proven
  byte-identical to a full `chunk_file` re-chunk by the 0038 unit tests.
- **`memory.bin` removal is safe:** nothing on the running-VM, resume, or
  restore path reads `snapshots/<id>/memory.bin` — the VM runs from
  anonymous RAM / UFFD-served chunks / COW-mapped restore source, and a
  cross-host restore re-materializes from the chunk store via the
  manifest. The dump file is purely the capture *output*.
- **Cold-cache capture regression is bounded + off the hot path:** with
  no rolling memfile, a checkpoint whose prev chunks have been
  cache-evicted fetches them from GCS (~87 ms each). This lengthens the
  post-resume re-chunk (and the `capture_lock` hold), never the
  guest-visible pause or resume. 0038's `try_lock`-skip (B1) keeps a slow
  capture from gridlocking the fleet.

## Further work (roadmap from the prod-latency investigation)

Driving real prod sessions (`0e22a4e9` + others) with the prod-ops tooling
produced a unified diagnosis: **slow eviction and slow resume are both
under-parallelized chunk I/O + lost cache locality** — not the FC capture,
the `capture_lock`, UFFD, or the disk (all measured fast). Roadmap, in
priority order:

1. **Async, single-flight chunk I/O.**
   - *Save (eviction):* `update_for_dirty_ranges_sparse`,
     `chunk_memory_to_store`, and the disk flush walk dirty chunks
     **serially** — 1,936 chunks → ~32 s, ~11.5 K → ~4 min (measured).
     Parallelize with bounded `buffer_unordered`.
   - *Load (resume):* `restore.prefetch_memory` eagerly pulls the **full**
     memory image (≈16 K × 512 KiB chunks for 8 GiB) at hardcoded
     **concurrency 8** before resume — **66.5 s of a 69 s cold resume**
     (traced). Make it async + move it **into the UFFD-handler process**
     (which already serves faults via the single-flight `ChunkCache.get`),
     so the VM resumes immediately and faults coalesce on the in-flight
     prefetch. The in-process NBD disk daemon gets the same treatment.
   - Close the blind spot: `snapshot_create_seconds` wraps only the FC
     capture, not the re-chunk/upload, so the slow phase is invisible.
2. **Cache locality.**
   - **PIN the enabled images' canonical (memory+disk) manifests** in the
     cache pin set at host **boot AND on image-enable** — `image_prefetch`
     currently *puts but does not pin* the base, so the LRU can evict it
     and a cold resume re-pulls the shared base from GCS. Pinned ⇒ a resume
     fetches only the **session-divergent** chunks.
   - **Publish the working-set trace** (the UFFD handler already *records*
     it via `WorkingSetRecorder`; the gap is the upload —
     `working_set_blob_key` is never set, so the prefetch narrowing falls
     back to full-manifest).
   - **Resume host-affinity** — prefer the original warm host over a cold
     cross-host node (observed: resume jumped `h31l → hwf1` despite the
     origin being alive; warm resume ~0.7 s vs 69 s cold).
3. **Disk-aware chunk-cache budget + thrash metrics.** The LRU exists
   (`evict_to_budget`, skips the pin set) but the default budget (200 GiB)
   exceeds the ~98 GiB host disk, so it never evicts before the disk fills.
   Default to a free-space floor (≈90 % disk, checked dynamically), with an
   optional absolute ceiling via `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`.
4. **Resume scheduling.** `ensure_active` 409-bounces a message that races
   a mid-eviction session instead of holding + auto-resuming; the 30 s
   `ENGRAM_IDLE_TTL_SECS` evicts sessions mid-think-pause (observed: a
   resumed session re-nominated for eviction 32 s later, no work done).
5. **`snapshots/<id>/` reaper** for the residual tiny `state.bin` +
   `manifest.json` dirs (the GiB `memory.bin` is gone after this ADR).
6. **Hygiene noticed in passing:** stale host-registry rows (403, mostly
   dead) + the FC SIGKILL-on-teardown ("firecracker didn't exit in time").
7. **Existing on-host disk** — a one-shot prod-ops cleanup of leftover
   pre-ADR-0039 `memory.bin`s (this code only stops *new* growth, after the
   FC-host MIG re-bakes + rolls).

## Roadmap implementation (follow-up PRs #96–#99)

The §1–§4 items above shipped as four follow-up PRs, all merged to `main`;
the chunk-I/O, locality, and resume-scheduling pieces were prod-validated
on the rolled FC fleet (2026-06-06, throwaway `demo-claude` `mode=dev_vm`
sessions driven via `/exec`). What landed, what's design-only, and what
still needs dev-vm/FC validation:

### §1 — Async, single-flight chunk I/O — PR [#97](https://github.com/cortexapps/engrams/pull/97) (`fix/chunk-io-concurrency`)

**Implemented** (bounded `futures::buffer_unordered`, concurrency 32):

- `chunk-store::file::update_for_dirty_ranges_sparse` — per-dirty-chunk
  `get_chunk(prev)` + diff-overlay + `put_chunk` run concurrently on private
  per-task diff handles; results folded into the carried `BTreeMap` after
  the concurrent phase (`None` elides → sparse invariant preserved).
- `chunk-store::file::chunk_file` (the memory re-chunk) — sequential reads
  off one handle, puts in concurrency-sized windows (RAM bounded at
  `concurrency × chunk_size`, no whole-image buffering); manifest stays
  offset-sorted.
- `host-agent::disk_daemon::backend::flush_upload` (NBD disk flush) — serial
  put loop fanned out at 32; manifest rebuilt only after every put succeeds
  (durability invariant), `chunk.flush` span kept.
- `host-agent::pooled_backend::prefetch_memory_chunks` — cold-resume
  prefetch concurrency 8→32 (`MEMORY_PREFETCH_CONCURRENCY`).

**Design-only** (high-risk, dev-vm-gated): make `restore.prefetch_memory`
non-blocking + move the prefetch into the `engram-uffd-handler` process so
lazy UFFD faults coalesce onto the in-flight `ChunkCache.get` — today
host-agent and uffd-handler are separate processes with separate
`ChunkCache`s and would double-fetch in-flight chunks. This is the
`66.5 s of a 69 s cold resume` win. It is **sound** (UFFD already lazy-faults
from durable chunks, and the resume path already falls back to lazy faulting
when the prefetch errors) and **de-risked by §2 pinning** (the cold-fault
surface shrinks to the per-session diff); the remaining work also needs a
missing-page fail-fast in the uffd-handler (else a missing page hangs the
guest, the UFFD analog of the NBD wedge fixed in #103).

**Prod-validated:** evict + warm resume fast, disk byte-identical across
evict→resume, no wedge/blob-not-found. (Large-dirty-set ~4 min serial
baseline comparison still open.)

### §2 — Cache locality: pin canonical base manifests + narrow prefetch — PR [#99](https://github.com/cortexapps/engrams/pull/99) (`adr0039-cache-locality-pin-base`)

**Implemented:**

- **Pin canonical base manifests.** The cache `pin`/`unpin` API existed but
  was never called, so the LRU could evict the shared base out from under
  live File-backend siblings. The image-prefetch supervisor now pins each
  enabled image's deduped base-manifest chunk set (disk + memory) via
  `ChunkCache::pin_all` after a successful warm at host boot AND
  image-enable, unpinning on disable. Pins are now **reference-counted**
  (`HashMap<ChunkHash, u32>` replacing `HashSet`) so two images sharing a
  base chunk both pin it; `evict_to_budget` skips pinned hashes.
- **Narrow base prefetch.** `try_restore_base_snapshot` now sets
  `working_set_blob_key` → `traces/<memory_manifest_id>/canonical.json` so
  `prefetch_memory_chunks` narrows to the working set when a canonical trace
  exists (full-manifest fallback when absent; `None` for disk-only/VZ).

**Design-only:** resume host-affinity (soft last-host preference /
`ScheduleContext.prefer_host`) — snapshot-affinity via `prefer_snapshot_id`
already covers the primary win; the soft preference touches the
evac/host-lost/`exclude_host` paths → multi-host FC dev-vm validation.

**Prod-validated:** rolled hosts log `"image base snapshot prefetched +
pinned; marked ready"` (demo-claude 280 chunks, dev-engrams 709);
`engram_chunk_cache_refetch_after_evict_total` stays 0 across evict→resume
(the pinned base is never evicted + refetched).

### §3 — Disk-aware chunk-cache budget + thrash metrics — PR [#98](https://github.com/cortexapps/engrams/pull/98) (`chunk-cache-disk-aware-budget`)

**Implemented** (self-contained to `chunk-store::cache`):

- `evict_to_budget` re-probes the cache filesystem via `statvfs(2)` each
  sweep and LRU-evicts oldest-first (skipping pinned) to hold the mount
  at/under ~90 % full (10 % free floor), dynamically yielding disk to
  snapshots/checkpoints sharing `work_dir` — fixes the prod incident where
  the fixed 200 GiB byte-budget never tripped on a ~98 GiB host.
- `budget_bytes` → optional absolute ceiling (`NO_CEILING` default lets the
  floor govern). Env: `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`,
  `_FREE_FLOOR_PCT` / `_FREE_FLOOR_BYTES`.
- Thrash metric: bounded `EvictedRing` (4096 hashes) +
  `engram_chunk_cache_refetch_after_evict_total`; `engram_chunk_cache_fs_free_bytes`
  gauge. `ChunkCacheConfig` kept struct-literal-compatible (floor resolved
  inside `ChunkCache::new`, not a new field) so downstream crates compile
  unedited.

**Prod-confirmed:** `fs_free_bytes` gauge + (absent ⇒ 0) thrash counter
scrape on the rolled host-agents. **Needs dev-vm/FC validation:** the floor
evicting oldest-first under real `work_dir` pressure without ENOSPC while
pinned working-set chunks survive **floor** (not just ceiling) pressure.
(`statvfs` works on macOS, so the floor math is exercised locally.)

### §4 — Resume scheduling: hold-not-bounce + saner idle TTL — PR [#96](https://github.com/cortexapps/engrams/pull/96) (`fix/resume-evicting-hold-and-idle-ttl`)

**Implemented:**

- **Hold-not-bounce.** `ensure_active`'s `Evicting` arm holds via a bounded
  poll (`ensure_active_after_evicting_hold[_for]`, default 8 s / 250 ms, env
  `ENGRAM_RESUME_EVICTING_HOLD_SECS`, `0` = opt-out) until the eviction
  lands the session at `Idle`/`Evacuating`, then dispatches the existing
  `resume_session()` inline; fallback after the bound is the same retryable
  409.
- **Saner idle TTL.** `DEFAULT_IDLE_TTL_SECS` 30→300 (env
  `ENGRAM_IDLE_TTL_SECS`; 30-min hard-TTL backstop unchanged).

**Prod-validated + known follow-up:** the idle-eviction state machine and
300 s TTL are confirmed, and a message arriving mid-eviction is retryable
(not stranded — a post-eviction retry resumes + delivers). The hold does
**not** reliably deliver single-shot, though: `evict_session_to_state`
transitions the session →`Idle` *before* its `SessionLeaseGuard` releases
(`Drop` spawns a detached release task), so the hold's `resume_session` can
observe `Idle` while the lease is still held and return the retryable
lease-409. Fix (deferred): have the hold retry the transient lease-conflict
within its deadline, or release the lease before/with the `Idle` transition;
confirm via the `ensure_active_after_evicting_hold_for` test seam.

### Roadmap coverage

| 0039 § | PR | State |
| --- | --- | --- |
| §1 Async, single-flight chunk I/O | [#97](https://github.com/cortexapps/engrams/pull/97) | Parallel I/O shipped; async cross-process prefetch design-only |
| §2 Cache locality | [#99](https://github.com/cortexapps/engrams/pull/99) | Pin + narrow shipped; resume host-affinity design-only |
| §3 Disk-aware budget + thrash metrics | [#98](https://github.com/cortexapps/engrams/pull/98) | Shipped |
| §4 Resume scheduling | [#96](https://github.com/cortexapps/engrams/pull/96) | Shipped (hold single-shot has a lease-race follow-up) |

§5 (`snapshots/<id>/` reaper), §6 (host-registry / FC SIGKILL hygiene), and
§7 (one-shot prod-ops cleanup of pre-0039 `memory.bin`s) remain unaddressed
and stay queued above.

## Implementation

Commit chain (this branch, off the ADR-0038 branch):

- `7e8ced1` ADR authored (Proposed)
- `0bd1028` all-sparse retirement: `seed_checkpoint_chain_sparse` for both
  resume AND the fresh Full capture; `memory.bin` removed after chunking
  (the 61G leak fix); diff branch always `update_for_dirty_ranges_sparse`;
  rolling path retired — `CheckpointChain.rolling_memfile`,
  `checkpoint_rolling_path`, the copy-variant `seed_checkpoint_chain`,
  `checkpoint::overlay_sparse`, and the chunk-store full-file
  `update_for_dirty_ranges` (+ its unit test). Net −246 LOC.
- ADR → Accepted with the dev-vm validation record.
- (flag flip) `restore_mode_from_env` default `file→uffd` +
  `base_restore_mode_from_env` default `inherit→file`; tests + coordinator
  comments updated. Added the "Feature-flag decisions" section + this
  "Further work" roadmap (folding in the prod-latency investigation).

(Early hashes are pre-rebase: the branch was rebased onto `origin/main`
after #93 merged, so live SHAs differ — see `git log`.)

## Divergences from this proposal

- **The chunk-store `update_for_dirty_ranges` (full-file re-chunk) was
  retired too**, not just the host-agent rolling path. Once the rolling
  memfile was gone it had no in-tree caller, and the sparse variant is
  proven byte-identical to it — so leaving it (plus `overlay_sparse`)
  would have been dead public API. Both were removed with their tests;
  ADR-0022 fork can reintroduce a local-image re-chunk deliberately if it
  needs one.

## Status

Accepted. #93 (ADR 0038) has merged to `main`; this branch was rebased
onto `origin/main`, so the PR bases on `main`. The chunk-I/O (§1, #97),
cache-locality (§2, #99), disk-aware-budget (§3, #98), and
resume-scheduling (§4, #96) items from **Further work** have all shipped —
see **Roadmap implementation** above for what landed vs. what stays
design-only/dev-vm-gated. §5–§7 remain queued.
