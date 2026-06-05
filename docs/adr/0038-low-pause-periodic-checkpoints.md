# ADR 0038: Low-pause periodic checkpoints — resume-seeded sparse chains + off-pause upload

Status: 2026-06-05 — **Proposed.** Authored before code (bookend
convention); flips to **Accepted** with the commit chain once
implemented and dev-vm-validated.

Continuation of **ADR 0028** (diff-first periodic checkpoints) and
**ADR 0022** (File vs UFFD memory backing). 0028 gave us the rolling
checkpoint chain + `SnapshotType::Diff` substrate; 0022 gave us the
File/UFFD `RestoreMode` split. This ADR fixes how the two compose on
the *resume → checkpoint* path, where a coupling neither anticipated
produced a prod incident.

## Context

Prod sessions were being snapshot-and-evicted **while actively
working** — the agent would emit "awaiting prompt" mid-task and go
idle — and evictions/resumes took minutes. Traced from session
`5fadd364-01b1-4919-80e3-1bcd14033902` (host `engrams-fc-0wqk`).

The idle-eviction itself was a *symptom* of the harness losing its
host connection (fixed separately in ADR-less PR #92 — the harness
decoupling). The *cause* of the connection loss, and a broader fleet
hazard, is the FC periodic-checkpoint capture. Three root causes,
each confirmed against Cloud Trace + the code:

1. **The 60s hang is the Full memory seed.** After an idle→active
   **UFFD** resume, `PooledBackend::checkpoint_chains` is empty (the
   restore path never seeds it; the chain is only ever seeded *after*
   a Full capture — `pooled_backend.rs`). So the first periodic
   checkpoint takes a **Full** memory capture, reading every guest
   page to dump it. Under UFFD those pages are lazy, so the read
   faults the entire working set in from the chunk store →
   `PUT /snapshot/create` blows the 60s FC-client timeout
   (`engram-sandbox-firecracker/src/lib.rs`). Trace evidence: healthy
   `Diff` checkpoints are ~2.7 s; the cold `Full` seed is the only one
   that hangs.

2. **`capture_lock` turns one hung capture into a fleet gridlock.**
   Captures serialize per-sandbox on a `tokio::sync::Mutex`; a
   periodic checkpoint *queues* (`.lock().await`) behind an in-flight
   one. Trace showed `host.snapshot` (eviction/evac) spans with
   **52–151 s of dark wait** before any work began, and a **299 s**
   resume — all stuck behind hung Full seeds.

3. **Even healthy checkpoints freeze the guest ~2.7 s uploading disk
   chunks to GCS *inside the pause*.** `disk_daemon::backend.rs::
   flush()` `put_chunk(&bytes).await`s every dirty chunk (a GCS
   upload) between FC pause and resume. The dirty bytes are already
   host-resident (copied into the `dirty` buffer at guest-write time)
   and chunks are content-addressed + immutable, so the upload has no
   business on the frozen-guest path.

## Decision

Adopt the target model the team converged on: **pause → fast local
capture → resume → upload deltas to GCS → done.** The guest-visible
pause should be O(local work); all GCS I/O happens after resume.
Realized in four pieces:

- **B0 — Instrument the capture.** No span/metric exists inside
  `create_snapshot` today; the UFFD page-in emits nothing. Add
  `engram_snapshot_create_seconds{type}`, `…capture_lock_wait_seconds
  {path}`, `engram_checkpoint_skipped_total`, and an `fc.create_snapshot`
  span so the fix is verifiable in prod and the blind spot is closed.

- **B1 — `capture_lock`: skip, don't queue (periodic only).** A
  periodic checkpoint `try_lock`s and bails if a capture is already in
  flight; eviction/evac/drain keep the blocking acquire (they *must*
  capture). One slow capture can no longer gridlock the fleet.

- **B2 — Seed the checkpoint chain on resume → first checkpoint is a
  diff.** On resume, seed the chain from the resume source's existing
  memory manifest, **manifest-only (no rolling memfile)** so UFFD's
  laziness is preserved. A new chunk-store primitive
  `update_for_dirty_ranges_sparse` re-chunks each dirty 512 KiB chunk
  by fetching its previous content by hash (warm local cache) and
  overlaying the sparse `memory.diff` — no full local image required.
  The first post-resume checkpoint is then a cheap diff over resident
  dirty pages; no Full read, no UFFD fault storm.

- **B3 — Move the disk-chunk GCS upload out of the pause.** Split the
  NBD `flush()` into `flush_local()` (drain + hash + rebase, under
  pause; bytes parked in a per-backend pending-upload map that the
  read path consults) and `flush_upload()` (GCS upload, post-resume,
  awaited before the checkpoint is *recorded* — durability without
  freezing the guest).

## Alternatives considered (and rejected)

- **Flip idle-resume to File mode.** File restore leaves a resident
  full memfile (cheap checkpoints), but it's only fast when the image
  is *locally resident* — the idle→active case is precisely when it
  isn't (post-evict, often cross-host), so File restore there becomes
  a synchronous multi-GB GCS reconstruct, re-slowing the exact
  transition UFFD exists to make fast. Keep UFFD for the cold
  reconstruct; fix the checkpoint instead.
- **Materialize a full rolling memfile on resume.** Reuses the
  existing diff path unchanged, but downloading/reconstructing the
  whole image at resume defeats UFFD's RAM-leanness. The sparse
  re-chunk (B2) gets the same cheap diff while keeping memory lazy.
- **Raise the 60s FC `create_snapshot` timeout.** Treats the symptom;
  the Full-seed fault storm scales with guest RAM and host
  contention. B2 removes the Full seed entirely.

## Invariants / correctness

- **Sparse re-chunk correctness:** for each dirty chunk,
  `prev_chunk ⊕ dirty_pages == current guest memory`. Holds because
  KVM dirty tracking is re-armed at `load_snapshot` for both File and
  UFFD (`lib.rs`), so `memory.diff` carries exactly the pages dirtied
  since the chain's prev capture (the resume source for the first
  diff); clean bytes come from the prev chunk. Omitted prev chunk =
  all-zero (UFFDIO_ZEROPAGE semantics).
- **B3 durability-before-record:** `snapshot()` awaits the disk +
  memory GCS uploads before returning, so the recorded checkpoint
  references durable manifests. The `capture_lock` is held through the
  post-resume upload, so an eviction can't reference a not-yet-durable
  version; a host crash mid-upload leaves the checkpoint *unrecorded*
  (coherent — it simply didn't happen).
- **Scheduler coexistence:** the background `flush_scheduler` keeps the
  full `flush()` (drain + upload inline — it doesn't pause the guest).
  The `dirty` mutex serializes it with the snapshot-path drain;
  content-addressing + the existing `VersionConflict` retry keep
  racing manifest versions safe. The ADR-0014 cross-host-evac canary
  (memory ahead of disk) is the class to watch in dev-vm validation.

## Implementation

Commit chain (this branch — to be filled as phases land):

- ADR authored (Proposed)
- B0: snapshot capture instrumentation
- B1: `capture_lock` skip-not-queue
- B2.1: `update_for_dirty_ranges_sparse` chunk-store primitive + tests
- B2.2–2.4: resume chain-seed (host-agent)
- B3: off-pause disk upload

## Divergences from this proposal

_(filled in at Accepted-flip.)_
