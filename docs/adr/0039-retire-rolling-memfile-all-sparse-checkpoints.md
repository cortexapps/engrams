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

## Out of scope (follow-ups)

- **`snapshots/<id>/` directory reaper.** After (2), the leftover
  per-snapshot dirs hold only tiny `state.bin` + `manifest.json`; they
  still accumulate with no reaper. A pin-set reaper (mirroring
  `orphan_reap`'s live-disk-manifest sweep) is a separate, lower-urgency
  PR.
- **Chunk-cache LRU** (the 21 G).
- **Existing on-host 61 G** — needs a one-shot prod-ops cleanup (this code
  only stops *new* growth, after the FC-host MIG re-bakes + rolls).

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
- (this commit) ADR → Accepted with the dev-vm validation record.

## Divergences from this proposal

- **The chunk-store `update_for_dirty_ranges` (full-file re-chunk) was
  retired too**, not just the host-agent rolling path. Once the rolling
  memfile was gone it had no in-tree caller, and the sparse variant is
  proven byte-identical to it — so leaving it (plus `overlay_sparse`)
  would have been dead public API. Both were removed with their tests;
  ADR-0022 fork can reintroduce a local-image re-chunk deliberately if it
  needs one.

## Status

Accepted. Stacked on the ADR-0038 branch (`fix/fc-snapshot-create-hang`,
PR #93); PR base = that branch so the two review + roll back independently.
