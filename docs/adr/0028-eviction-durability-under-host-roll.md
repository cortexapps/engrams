# ADR 0028: Eviction durability under coord restart + host roll

Status: 2026-06-03 — **Proposed (revised).** Incident-driven. Authored
before code per the team's ADR-bookend norm; revised before
implementation after (a) [ADR 0034](0034-idle-eviction-control-plane-and-detection.md)
landed and closed the **control-plane** half of Defect A, and (b) design
review settled the checkpoint mechanics on **diff-first periodic
checkpoints on stock Firecracker** — KVM dirty-page tracking + Diff
snapshots + the content-addressed chunk store — rather than the original
"coarse cadence, full pause + full dump" first cut. That same review
concluded the checkpoint artifacts should be **fork-ready by
construction** (forking sessions is on the product horizon) and that
[ADR 0022](0022-runtime-memory-sharing-and-forking.md)'s Option B (the
`direct-mem` FC fork) is **rejected for now** — see "Relation to
ADR 0022". The fix lands as independently shippable phases (commit
chain at the end). The core move is unchanged: make recovery rest on a
**periodic, durably-recorded coherent (memory, disk) checkpoint** rather
than a suspend-only memory snapshot. Supersedes nothing; extends
[ADR 0018](0018-session-evacuation.md) (evacuation),
[ADR 0016](0016-cow-observability-and-continuous-sync.md) (continuous
disk sync + idle-eviction ordering), [ADR 0009](0009-state-reconciliation.md)
(graceful shutdown), and [ADR 0034](0034-idle-eviction-control-plane-and-detection.md)
(the `Evicting` lane this slots into).

## Context: the incident (session `cf4d4afd`, 2026-06-02)

A user's session worked well, went idle, then on the next follow-up
returned `forward prompt to harness: sandbox not found`. Investigation
(prod-ops, coord + host-agent logs + PG + GCE) reconstructed this:

| Time (UTC) | Event |
|---|---|
| 14:53:08 | Session created; sandbox `102aa03e` restored on host `dabc0132` (VM `engrams-fc-b158`). Active work; disk continuously flushed to GCS (`dd030aa5`, → v76). |
| 14:57:33 | Agent finishes its run (`harness_idle`). |
| **14:58:06** | Idle-eviction begins: coord's `evict_session_to_state` RPCs `host.snapshot(102aa03e)`. Host pauses the VM, flushes the disk post-pause (v77), and starts uploading the FC memory/state chunks to GCS. This RPC takes ~2 minutes. |
| ~15:00:03 | Host's next `/idle-eviction-candidates` POST to coord **times out** — coord is already draining for a deploy. |
| ~15:01 | **Coord rolls** to image `a9401280` (PR #57, an FC-guest change). The old coord pod is SIGTERM'd. |
| 15:06–15:08 | **FC-host MIG rolls** (PR #57 re-baked the host image). `b158` + its peer are destroyed; replaced by fresh hosts with empty local snapshot dirs. |
| 15:08:57 | Coord's dead-host detector marks `dabc0132` dead → session orphaned `HostLost → Evacuating`. |
| 15:08–15:12 | `evac_resumer` attempts 1–20, each failing `read fc manifest.json …/snapshots/<id>/manifest.json: No such file or directory`. |
| ~15:12 | Budget exhausted → session falls back to `Idle`; in-RAM state lost. (Disk `dd030aa5@v76` survives in GCS; the agent's PR was already pushed.) |

**Confirmed forensic facts:**
- PG `snapshots` table held **0 rows** for the session — the snapshot
  was never recorded.
- The Firecracker + uffd-handler processes on `b158` were **still
  alive** (started 14:53:07, never `destroy`'d) and the VM had been
  **paused since 14:58:06** (disk flushes stopped there) — i.e. the
  eviction pipeline halted between `host.snapshot()` returning and
  `record_snapshot()` / `destroy()`.
- The session row carried `live_disk_manifest dd030aa5@v76` but no
  `base_snapshot`/memory snapshot.
- Contrast: ADR 0018's prod validation shows sessions **with** a
  recorded snapshot survive MIG rolls (session `3e692ab6` survived
  three). The gap is specific to *interrupted* eviction + *disk-only*
  recovery, not evac in general.

## Root cause: three independent defects

### Defect A — coord-orchestrated eviction is not restart-safe

> **Post-0034 status:** ADR 0034 (PR #71) closed the **control-plane**
> half of this defect: nomination now flips `Active → Evicting` (a
> durable PG intent marker) and a coord-side scanner re-drives the
> pipeline after any coord restart, with a retry budget falling back to
> `HostLost`. What remains open — and what Fix A addresses — is the
> **data-plane** half: the scanner *re-runs the whole snapshot from
> scratch* because nothing reconciles a snapshot that already reached
> GCS but never got its PG row. The analysis below describes the
> original incident-time code; the durability hole it identifies is the
> unrecorded-artifact window, which 0034 deliberately left to this ADR.

`idle_evictor::evict_session_to_state`
(`crates/engram-coordinator/src/idle_evictor.rs`) runs the pipeline
synchronously in one coord task: `host.snapshot()` (the long RPC) →
`record_snapshot()` (PG) → `transition_session` → `destroy()`.

The ordering within the pipeline is careful and correct (record-before-
destroy, PG-before-destroy, in-flight commit/abort tracking). But if
coord exits during the ~2-minute `host.snapshot()` RPC (exactly what a
deploy roll does):

- the host *completed* the snapshot and uploaded the chunks to GCS,
  but coord never reached `record_snapshot` → **no PG row**;
- coord never reached `destroy` → the **source VM is left paused**
  (orphaned, as observed);
- the host's in-flight-snapshot tracking is neither committed nor
  aborted.

The uploaded GCS artifacts are keyed by a `SnapshotId` that only ever
existed on the now-dead host and in the dropped coord task — they are
unreferenceable. The session is left with only its `live_disk_manifest`.
Post-0034 the *eviction intent* survives, but there is still no
reconciliation that re-records artifacts already in GCS — the re-driven
pipeline pays the full snapshot again, and if the *host* (not just
coord) is gone before any attempt completes, the artifacts are stranded
exactly as in the incident.

### Defect B — disk-only evacuation is unimplemented (silently fails 20×)

When a session reaches `Evacuating` with a `live_disk_manifest` but no
snapshot row, `dead_host` still routes it to `Evacuating` (correct: it
*does* have recoverable disk — `dead_host.rs` checks
`snapshot.is_some() || has_live_manifest`). But the resume path can't
honor it:

- `evac_resumer::run_resume_pipeline` calls
  `latest_snapshot_for_session` → `None`, then
  `evacuate_dead_source(.., snapshot = None)`.
- `evacuate_dead_source` (`evacuation.rs`) mints a **fresh `SnapshotId`**
  with `state_blob_key = None, sidecar_blob_key = None` and calls
  `target_backend.restore(metadata)`.
- The host-side `FirecrackerBackend::restore` unconditionally does
  `tokio::fs::read(snapshot_dir_for(metadata.id).join("manifest.json"))`
  (`engram-sandbox-firecracker/src/lib.rs`). With no blob keys,
  `materialize_state_if_missing` downloads nothing, so `manifest.json`
  never exists → `RestoreFailed` every attempt.

So the `EvacLoss::Memory { reason: "source-dead-no-snapshot" }` case
that the types anticipate has **no working restore path**. A full-FC
restore requires a memory snapshot + `manifest.json`; a disk-only
session has neither. The resumer burns all 20 attempts (~3 min of
churn + log noise) on a structurally impossible operation, then drops
to `Idle`, where a manual `/resume` hits the same wall and ultimately
`transition_to_dead_if_no_snapshot`.

A disk-only session *is* recoverable in principle, but **not** by
reusing `restore_base_for_session`: that resumes the base **memory**
snapshot, and pairing base memory with the session's *evolved* disk is
**incoherent** (the restored RAM's page cache + mounted-fs metadata
describe the base disk, not the evolved one → corruption). The coherent
recovery is a **fresh kernel boot** that mounts the recovered rootfs —
the `live_disk_manifest` is a *full* rootfs, so a cold boot against it
is coherent (clean mount, fresh harness; in-RAM/process context lost).
That primitive composes existing parts (materialize manifest → NBD
rootfs → FC `create`) but is not wired into evac today.

### Defect C — the deploy terminates hosts without draining sessions

PR #57 changed the FC guest, which auto-triggers a host-image re-bake
+ MIG roll (per the deploy auto-trigger design). The MIG replaces hosts
by deletion; nothing drains their live/idle sessions first.

The host-agent *has* a SIGTERM checkpoint pipeline
(`engram-host-agent/src/shutdown.rs`, ADR 0009 Phase 7) that snapshots
each live sandbox on signal — but (a) it updates the local
`sandbox.json` and does not durably *record the snapshot in coord/PG*,
so it has the same unreferenceable-artifact problem as Defect A when
the host is then deleted; and (b) the GCE shutdown grace period is far
shorter than the ~2-minute snapshot+upload observed here, so the
checkpoint can be cut off. There is an operator drain endpoint
(`POST /api/admin/hosts/:id/drain`) that cordons + evacuates to
`Evacuating` and waits — but the deploy tooling never calls it.

### The deeper finding — disk is continuous, memory is suspend-only

Underneath all three defects is a **cadence asymmetry**:

- **Disk is continuously synced.** The `FlushScheduler` (ADR 0016
  Phase B) flushes dirty chunks to GCS on a periodic tick / 256 MiB
  threshold and publishes an advancing `live_disk_manifest`
  (`dd030aa5 → v76/v77` in the incident). Disk is durable + recent
  independent of any snapshot.
- **Memory is captured only at suspend** — inside `pooled_backend::snapshot()`,
  which runs *only* at idle-eviction, operator drain, and SIGTERM
  shutdown. There is **no periodic memory checkpoint**.

So a session in steady state has a recent durable disk but **no
durable coherent (memory, disk) pair**. The incident is the corner of
that asymmetry: the one memory capture at eviction never got recorded,
leaving disk-only state — and disk-only can only ever cold-recover
(all in-RAM/agent context lost). The fix isn't just "make the eviction
snapshot crash-safe"; it's to **periodically capture a durable,
coherent checkpoint** so host loss costs minutes, not the whole session.

## Decision

Re-anchor recovery on a **periodic, host-driven, durably-recorded
coherent checkpoint**, with cold-boot-on-latest-disk as the fallback
rung and drain-before-delete as prevention. Smallest-blast-radius
first; A, B are in this OSS repo, C spans `engrams-internal`.

### Recovery ladder (target end-state)

On sandbox/host loss, recover to the best available rung:

1. **Last coherent checkpoint → warm resume.** A periodic (memory,
   disk) pair captured atomically and durably recorded. **Rewind** the
   disk to that checkpoint's version — discarding the continuous-sync
   deltas that landed *after* it — to keep the (memory, disk) pair
   coherent. Cost: ≤ one checkpoint interval of lost work; **agent
   RAM / conversation / process context preserved.** Best UX.
2. **Cold boot on the latest disk (Fix B).** No usable coherent
   checkpoint, but the continuous-sync `live_disk_manifest` is current:
   fresh kernel boot mounting the recovered rootfs, fresh harness.
   Newest on-disk files, **memory/context lost.**
3. **Floor.** Disk in GCS / work already pushed to the forge (the PR).

**Coherence rule for rung 1 (load-bearing, easy to get wrong):** a
rung-1 restore must pair the checkpoint's memory with the checkpoint's
**own** disk manifest version — explicitly *not* the newer
`live_disk_manifest`. The existing live-wins preference in
`pick_evac_disk_manifest` stays correct only for rung 2, where a fresh
kernel mounts whatever disk it's given. Pairing checkpoint memory with
a newer disk reproduces Defect B's corruption in miniature.

### Checkpoint mechanics — diff-first on stock Firecracker

The original draft assumed every checkpoint costs a full pause + full
memory dump, capping cadence at minutes and deferring anything better
to ADR 0022's FC fork. Design review found stock FC already has the
needed primitive, and our chunk store already has the rebase mechanism:

- **Dirty-page tracking is stock.** `track_dirty_pages` at boot /
  `enable_diff_snapshots` at `snapshot/load` turns on KVM dirty-log
  tracking; `PUT /snapshot/create` with `snapshot_type: Diff` writes
  *only pages dirtied since the last capture* to a sparse local file
  and resets the bitmap. Chained diffs are supported.
- **The chunk store is the rebase.** The host keeps a **rolling
  per-session `memory.bin`** on NVMe (the diff-apply target). After
  each capture it overlays the sparse diff onto the rolling file,
  re-chunks **only the chunks the dirty pages touched**, uploads
  content-addressed (already-present hashes skipped), and publishes
  memory manifest vN+1. Manifests remain **full-image chunk lists** —
  every checkpoint is independently restorable with no chain replay;
  deltas exist only at capture time. At-rest cost per checkpoint ≈ the
  dirty delta (same property the disk side has had since ADR 0016).
- **Pause covers capture, never upload.** The atomic window is the
  proven ADR 0018 12m ordering, unchanged:
  `pause → drain FlushScheduler dirty disk chunks (manifest vK) →
  FC Diff capture → resume`. Everything slow — chunk/hash/dedup/upload/
  record — runs async off the immutable local staging copy after
  resume. If the async tail fails, manifest vN+1 simply isn't
  published and the previous checkpoint remains the anchor; nothing is
  torn. Expected pause ≈ O(dirty set) ≈ O(100 ms) at minute-scale
  cadence (FC requires the pause for `snapshot/create` regardless, and
  the (memory, disk) pair needs a frozen instant anyway).
- **Baselines:** the first checkpoint after a fresh kernel boot is a
  Full capture (no baseline); after a snapshot restore, diffs compose
  against the restored manifest as baseline.
- **Eviction becomes "final diff checkpoint + destroy".** The
  eviction-time snapshot is just the last checkpoint in the chain — a
  small delta, not a multi-GiB dump. The `Evicting` 409 window a user
  can hit when prompting mid-eviction collapses from ~2 minutes to
  seconds. The 0034 pipeline's invariants are untouched.
- **Chunk-amplification knob.** Dirty tracking is 4 KiB pages; chunks
  are far coarser, so one stray dirty page re-uploads its whole chunk.
  Memory chunk size is a tunable measured in the spike (Phase 1) —
  smaller chunks trade per-chunk overhead against amplification.
- **Clock discipline (new steady-state requirement).** Paused vCPUs
  don't tick; ~100 ms lost per checkpoint *accumulates* (unlike the
  one-shot restore skew the clock-steering crate already fixes at
  resume). agentd gains a periodic re-step of `CLOCK_REALTIME` from the
  host's KVM PTP clock (`/dev/ptp0`) — same crate, new cadence — so the
  drip never reaches SigV4/TLS-breaking territory.

### Fix A — periodic coherent checkpoint + host-owned durable record + reconcile

The host is the durable owner of "a coherent snapshot exists in GCS";
coord's PG row is a cache of that fact, not its origin.

1. **Host-driven periodic coherent checkpoint** per the mechanics
   above, on an env-tunable cadence (start ~60 s; the pause cost makes
   tighter cadences viable, measured before tightening). The same
   capture path runs at suspend (eviction, drain, SIGTERM) as the final
   chain entry.
2. **Host persists a durable, self-describing record** of every
   completed checkpoint (sandbox_id, session_id, snapshot_id, disk +
   memory manifest refs, blob keys, `events_cursor`, recoverable,
   captured-at) the moment the upload finishes — a `snapshot.json`
   sibling of `sandbox.json`, extending the partial shape the SIGTERM
   path already writes — surviving the RPC reply / coord being lost.
3. **Host re-advertises un-acked checkpoints** in heartbeat /
   registration (extending the existing `LocalSnapshotReport` /
   `rehydrate_sandboxes` surfaces). Any coord reconciles them into PG
   via `record_snapshot` (idempotent on `snapshot_id`), so a checkpoint
   that reached GCS becomes a PG row regardless of which coord (if any)
   survived — *before* any host-deletion can strand it. This subsumes
   the "coord died mid-eviction" window (the interrupted eviction's
   snapshot is just an un-acked checkpoint the reconciler picks up) and
   makes the SIGTERM path durable for free.
4. **Retention + GC.** Checkpoint manifests join the existing pin-set
   GC (ADR 0016 Phase C): the **latest checkpoint per live session is
   always pinned** (the rung-1 anchor); older checkpoints stay pinned
   through an env-tunable retention window, then age out — only chunks
   *not shared with a newer checkpoint* ever become GC candidates, so
   the window's true cost is the divergence between checkpoints, not
   full images.

Net: every active session always has a recent, coherent, recorded
checkpoint to rewind to — recovery rung 1 is reachable — and the
checkpoint chain is cheap enough to run continuously.

### Fork-readiness (deliberate shaping, not scope)

Session forking is on the product horizon, and the checkpoint chain is
its natural substrate: a **zero-disturbance fork** restores N children
from the latest checkpoint without touching the parent (staleness ≤ one
cadence interval), and a **fresh fork** takes an on-demand diff
checkpoint first (~tens-of-ms pause, zero staleness). Three Fix A
artifacts are therefore shaped fork-ready from day one, at near-zero
extra cost:

- the **rolling per-session `memory.bin`** doubles as the future
  fork / File-backend-restore source (and, sooner, a same-host
  idle-resume fast path that skips chunk re-materialization);
- the **`events_cursor` watermark** on every checkpoint is also the
  fork's transcript cut-point;
- the **retention window** is also the forkable-history window, and a
  fork pins its fork-point via the child's snapshot row with no new GC
  machinery.

Fork itself — session re-identity (agentd bind / `session_env`
re-stamp, network identity, mid-run duplicate-side-effect semantics) —
is explicitly out of scope here and gets its own ADR.

### Fix B — cold-boot disk recovery (ladder rung 2)

In `evacuate_dead_source`, when no coherent checkpoint is usable but a
disk manifest is current, recover via a **fresh kernel boot mounting
the recovered rootfs** (new `restore_disk_only_for_session` primitive:
materialize `live_disk_manifest` → NBD rootfs → FC `create` → inject
`session_env`; caller runs `start_agent` for a fresh harness). NOT a
`restore_base_for_session` (base memory + evolved disk is incoherent —
see Defect B). Mark `EvacLoss::Memory { reason: "source-dead-no-snapshot" }`.
On-disk work survives; in-RAM context does not.

Plus a **fail-fast guard**: when neither a coherent checkpoint nor a
disk-only cold boot is possible, route to `Idle`/`Dead` immediately
with a clear reason instead of burning the 20-attempt budget on
`RestoreFailed` (`evac_resumer` classifies structural vs transient
EvacError).

### Rewinding the conversation log (a rung-1 consequence)

A rung-1 rewind recovers a coherent *guest* state (memory + disk) at
checkpoint time `T`. But the PG `session_events` log — the user-facing
transcript and the SSE history — is written in **real time** as the
agent emits `run_started` / `agent_message` / `tool_call_*` /
`pull_request_opened`, so at recovery it holds events up to the crash
time `T+Δ`. The restored guest has no memory of events in `(T, T+Δ]` and
will resume *from `T`*. Left unhandled, the transcript shows messages
and tool calls the resumed agent never made (from its perspective) and
may now re-do or diverge from — confusing, and a duplicate-work hazard.

So coherence is a **triple**, not a pair: a checkpoint must capture
`(memory, disk, event-log cursor)` atomically. Concretely:

1. **Watermark the checkpoint.** At pause, record the session's
   `session_events` high-water-mark (last event seq/id) alongside the
   memory + disk manifests. The checkpoint row gains an
   `events_cursor`. (Fork-ready: this is also the fork cut-point.)
2. **Rewind the log on resume, don't destroy it.** On a rung-1 restore,
   events after `events_cursor` are **tombstoned** (a `rewound_at` /
   recovery-epoch marker), not hard-deleted — the full history stays
   for audit. The live head (what the transcript renders as the
   continuing thread, and what new events append after) resets to the
   cursor. Subsequent events carry an incremented recovery epoch so the
   timeline is unambiguous.
3. **External side-effects are NOT rewound — and must be surfaced.**
   Only local guest memory + disk roll back. Anything the agent did in
   `(T, T+Δ]` that touched the outside world — `git push`, an opened PR,
   a sent message, a non-idempotent API call — *already happened* and
   survives the rewind. The resumed agent doesn't know it did them and
   may repeat them. This is the genuinely hard part; the platform can't
   undo it, so it must make it **visible** (see UX).

Rung-2 (cold boot on the latest disk) does **not** have this mismatch:
the newest disk is paired with the newest transcript, so no rewind is
needed there — but it loses all in-RAM context. (Open question for
implementation: where the harness's *conversation* context actually
lives — in guest RAM only, persisted to the rootfs, or replayable from
PG — determines whether rung-2 resumes "remembering" the thread or
starts fresh against the recovered files. This informs how much of the
log a rung-2 recovery should replay vs. present as a fresh segment.)

### UX for a rewind

The recovery must be **honest and legible**, not silent:

- Render a clear boundary in the transcript: *"↩ Recovered from a
  checkpoint after a host failure. ~M minutes / N messages after this
  point were rolled back; the agent resumed from here."* The rolled-back
  span stays viewable (collapsed/greyed) for transparency, not deleted.
- Call out **surviving side-effects** explicitly when detectable from
  the rolled-back events (e.g. *"A pull request was opened in the
  rolled-back span and still exists"* from a tombstoned
  `pull_request_opened`). The agent itself should be told, on resume,
  that it recovered from a checkpoint and that some prior actions may
  have completed externally — so it can re-check state (e.g. `git
  status`, "does my branch already exist?") rather than blindly redo.
- Diff-first cadence bounds `Δ`: the worst-case rewind the user ever
  sees is one checkpoint interval (~a minute), which also keeps the
  rolled-back span — and the surviving-side-effect surface — small.

### Fix C — drain hosts before the MIG deletes them

Two layers, defense in depth:

- **Deploy tooling (`engrams-internal`):** before `tf-apply` rolls the
  FC-host MIG, call `POST /api/admin/hosts/:id/drain` for each
  outgoing instance and wait for its sessions to reach a terminal
  evac state (or a timeout), so sessions relocate to a peer *while
  both hosts are alive* (the path ADR 0018 proved works in ~24s).
- **Host-agent SIGTERM (this repo):** the Phase-7 checkpoint now rides
  Fix A's capture + durable-record + re-advertise path, so an
  *ungraceful* termination still leaves recoverable, **recorded**
  state; the shutdown deadline only needs to cover a diff capture +
  local staging (small), with the GCE grace period sized accordingly.

Reliability-first ([reliability_and_latency_first]): the deploy must
not be able to silently eat a live session. Draining-before-delete is
the prevention; Fix A/B are the safety net when prevention is bypassed
(crash, preemption, partition).

## Relation to ADR 0022

The original draft deferred cheap checkpoints to 0022 Option B
(`direct-mem`: memfd + `UFFDIO_CONTINUE`, a maintained FC fork) for its
online dirty-page tracking. Design review **rejects Option B for now**,
from two directions at once:

- **Diff snapshots take its dirty-tracking pitch.** Stock FC's KVM
  dirty-log + Diff capture + our chunk store deliver O(dirty-set)
  checkpoints without owning the security-sensitive VMM memory manager.
- **Checkpoint-anchored snapshot-fork takes most of its fork pitch.**
  With a continuous checkpoint chain, "fork session X" is a restore off
  X's latest (or an on-demand) checkpoint — zero-disturbance at ≤ one
  cadence staleness, or ~tens-of-ms parent pause at zero staleness.
  Option B's residual exclusive is zero-pause *and* zero-staleness
  simultaneously — revisit only if that becomes a hard product
  requirement.

0022's **Option A** (File-backend `MAP_PRIVATE` density) is untouched
by this and proceeds as the follow-on arc; it consumes the artifacts
Fix A builds (rolling memfiles, manifest chains, memfile-granularity
pinning). The Phase 1 spike below measures both ADRs' load-bearing
assumptions in one harness.

## Consequences

- Active/idle sessions survive coord rolls and FC-host MIG rolls with a
  **warm** recovery to a recent coherent checkpoint (rung 1) — bounded
  work-loss (≤ one cadence interval), agent context preserved — falling
  back to cold-boot (rung 2) only when no checkpoint is usable. The
  ADR 0018 promise ("a host dies, the session keeps working") extends
  to the deploy-roll + interrupted-eviction case, and *keeps memory* in
  the common case.
- Periodic checkpointing adds a recurring guest **pause** per sandbox of
  O(dirty set) — expected ~100 ms at minute cadence, not the original
  full-dump minutes — plus a steady-state KVM dirty-tracking overhead
  while the VM runs. Both are measured in the spike before the cadence
  is fixed; cadence stays env-tunable.
- Eviction itself gets fast (final-diff + destroy), collapsing the
  `Evicting` 409 window from ~2 min to seconds — a user-visible latency
  win that falls out of the durability work.
- New host→coord surface (checkpoint inventory in heartbeat) + a
  reconciler tick; both idempotent, both single-coord-pod safe by the
  same lease/CAS patterns as the existing evac scanners.
- The `session_events` log becomes **epoch-versioned**: a rung-1 rewind
  tombstones a tail span rather than appending monotonically. Consumers
  (transcript render, SSE replay, any analytics over the event stream)
  must honor the recovery epoch / live head. Audit history is retained.
- Side-effects in the rolled-back window survive the rewind; the
  platform surfaces them but cannot undo them — a deliberate
  at-least-once posture for agent actions, made legible rather than
  hidden.
- Guests need steady-state clock discipline (periodic PTP re-step in
  agentd) — a session-image re-bake + enable to roll out.
- Storage: checkpoint chains are dedup-incremental at rest; the
  retention window is a storage-cost knob (and, later, the forkable
  history), with GC riding the existing pin-set machinery.
- Cross-repo coordination for Fix C (this repo + `engrams-internal`),
  so Fix C ships behind the deploy change; everything else is
  independently shippable here.

## Status / phase chain

To be checked off as phases land (per the ADR-bookend norm); spike and
prod-validation numbers recorded here as they arrive:

- [x] **P0** — this revision (Proposed, diff-first design).
- [x] **P1 (spike, dev-vm)** — `tests/diff_snapshot.rs` +
      `tests/file_restore_shared_rss.rs` (now permanent CI gates in the
      unprivileged FC lane). **Numbers (256 MiB guest, dev-vm):**
      - Full capture pause **2,090 ms** vs Diff capture **38 ms**
        (cold-boot `track_dirty_pages`) / **53 ms** (post-restore via
        `enable_diff_snapshots` at load) — **~40–55× less pause**, and
        the chain survives restores (the steady-state shape).
      - Diff is genuinely sparse: **3–4%** of guest RAM allocated for a
        ~16 MiB dirty set + churn.
      - Sparse-overlay rebase onto a rolling memory.bin is
        byte-faithful: sha256-verified markers across full+diff1 and
        chained full+diff1+diff2 restores.
      - File-backend page sharing (the ADR 0022 go/no-go): 3 siblings
        off one memory.bin, each RSS 87.8 MiB with **Shared_Clean
        84.7 MiB**; **Σpss/Σrss = 35%** vs the perfect-3-way floor of
        33%. Restores 56–66 ms (unjailed test path).
      - Memory chunk size: keep the existing **512 KiB** (worst-case
        4 KiB-page → chunk amplification is 128× but only ~0.5 MiB
        absolute per stray page; revisit only if prod upload metrics
        say otherwise).
      - Dirty-tracking steady-state overhead: not isolatable in a
        30 s test; watch in prod via the checkpoint pause/CPU metrics
        before tightening cadence below ~60 s.
- [ ] **B** — `restore_disk_only_for_session` cold-boot primitive +
      `evacuate_dead_source` rung-2 dispatch + fail-fast classification
      in `evac_resumer`. CI: disk-only recovery e2e (the `cf4d4afd`
      shape), fail-fast budget tests.
- [ ] **A** — FC diff plumbing; host periodic checkpoint task + rolling
      `memory.bin` + durable `snapshot.json` record + heartbeat
      re-advertise; coord reconciler; migration(s) incl.
      `snapshots.events_cursor`; GC retention pinning; agentd periodic
      clock re-step; eviction = final-diff. CI: checkpoint correctness
      + chained-diff restores, reconciler idempotency, coord-SIGKILL
      mid-eviction e2e, clock tolerance, GC retention.
- [ ] **A.log** — rung-1 rewind tombstones post-cursor `session_events`
      (recovery epoch) + resets the live head; transcript/SSE rewind
      boundary + surviving-side-effect surfacing; resumed agent is told
      it recovered. CI: rewind + epoch-consistent SSE + live-PG
      round-trips.
- [ ] **Rung-1 wiring** — evac prefers latest coherent checkpoint;
      rung-aware disk pick (checkpoint's own version, never newer
      live); ladder fall-through. CI: two-host host-kill e2e (memory
      intact, disk rewound, transcript rewound), corrupted-checkpoint
      fall-through, coherence guard.
- [ ] **C** — SIGTERM path rides Fix A record/re-advertise + deadline
      sizing (this repo); drain-before-roll in `engrams-internal`
      deploy tooling + GCE grace bump.
- [ ] **Bookend** — flip to Accepted with the commit chain + prod
      validation matrix results (coord roll during eviction; MIG roll →
      rung-1 with memory intact + disk rewound; rung-2 fidelity;
      pause/dirty-tracking overhead; `Evicting` 409 window in seconds);
      hand off measured context to ADR 0022.
