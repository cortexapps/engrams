# ADR 0028: Eviction durability under coord restart + host roll

Status: 2026-06-02 — **Proposed.** Incident-driven. Authored before
code per the team's ADR-bookend norm; the fix lands in three
independent commits (one per failure below) that can ship and roll
separately. The core move is to make recovery rest on a **periodic,
durably-recorded coherent (memory, disk) checkpoint** rather than a
suspend-only memory snapshot — see "The deeper finding" below.
Supersedes nothing; extends [ADR 0018](0018-session-evacuation.md)
(evacuation), [ADR 0016](0016-cow-observability-and-continuous-sync.md)
(continuous disk sync + idle-eviction ordering), and
[ADR 0009](0009-state-reconciliation.md) (graceful shutdown); the
checkpoint-cadence lever ties into [ADR 0022](0022-runtime-memory-sharing-and-forking.md)
(dirty-page tracking).

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

`idle_evictor::evict_session_to_state` (`crates/engram-coordinator/src/idle_evictor.rs:63`)
runs the whole pipeline synchronously in one coord task:
`host.snapshot()` (the long RPC, line 146) → `record_snapshot()` (PG,
line 176) → `transition_session` (line 223) → `destroy()` (line 240).

The ordering within the pipeline is careful and correct (record-before-
destroy, PG-before-destroy, in-flight commit/abort tracking). But the
pipeline as a whole has **no durability across a coord process exit**.
If coord receives SIGTERM during the ~2-minute `host.snapshot()` RPC
(exactly what a deploy roll does), the task is dropped:

- the host *completed* the snapshot and uploaded the chunks to GCS,
  but coord never reached `record_snapshot` → **no PG row**;
- coord never reached `destroy` → the **source VM is left paused**
  (orphaned, as observed);
- the `SessionLeaseGuard` Drop release is a detached `tokio::spawn`
  that may not run during runtime shutdown;
- the host's in-flight-snapshot tracking is neither committed nor
  aborted.

The uploaded GCS artifacts are keyed by a `SnapshotId` that only ever
existed on the now-dead host and in the dropped coord task — they are
unreferenceable. The session is left with only its `live_disk_manifest`.

There is no reconciliation that re-records or re-drives an
eviction that was in flight when a coord pod died.

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
`sandbox.json` and may not durably *record the snapshot in coord/PG*,
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
   coherent. Cost: minutes of lost work; **agent RAM / conversation /
   process context preserved.** Best UX.
2. **Cold boot on the latest disk (Fix B).** No usable coherent
   checkpoint, but the continuous-sync `live_disk_manifest` is current:
   fresh kernel boot mounting the recovered rootfs, fresh harness.
   Newest on-disk files, **memory/context lost.**
3. **Floor.** Disk in GCS / work already pushed to the forge (the PR).

### Fix A — periodic coherent checkpoint + host-owned durable record + reconcile

The host is the durable owner of "a coherent snapshot exists in GCS";
coord's PG row is a cache of that fact, not its origin.

1. **Host-driven periodic coherent checkpoint.** On a timer (and at
   suspend), the host runs the existing atomic `snapshot()` shape
   (`pause → flush disk post-pause → capture memory → upload both
   chunked → resume`) so the captured (memory, disk) pair is coherent
   by construction. Cadence is coarse to start (minutes) because stock
   FC requires a full pause + full memory capture per checkpoint;
   chunked-memory content-dedup (ADR 0007/0021) already keeps the
   *at-rest* cost incremental.
2. **Host persists a durable, self-describing record** of every
   completed checkpoint (sandbox_id, session_id, snapshot_id, disk +
   memory manifest refs, blob keys, recoverable, captured-at) the
   moment the upload finishes — surviving the RPC reply / coord being
   lost (extend `sandbox.json` / a sibling `snapshot.json`, partially
   done by the shutdown path).
3. **Host re-advertises un-acked checkpoints** in heartbeat /
   registration (`rehydrate_sandboxes` already carries a sandbox
   inventory). Any coord reconciles them into PG via `record_snapshot`
   (idempotent on `snapshot_id`), so a checkpoint that reached GCS
   becomes a PG row regardless of which coord (if any) survived —
   *before* any host-deletion can strand it. This also subsumes the
   "coord died mid-eviction" window: the interrupted eviction's
   snapshot is just an un-acked checkpoint the reconciler picks up.

Net: every active session always has a recent, coherent, recorded
checkpoint to rewind to — recovery rung 1 is reachable.

**Future cadence lever (ADR 0022).** Stock FC forces a full pause +
full memory dump per checkpoint, capping cadence at minutes. ADR 0022's
**Option B (`direct-mem`, memfd + `UFFDIO_CONTINUE`)** lists *online
dirty-page tracking → faster Pause / smaller diff-checkpoints* — exactly
what makes **frequent, near-zero-pause diff checkpoints** (seconds)
practical. 0022 is Proposed/parked and its primary thrust (memory
sharing/forking for density) is orthogonal; the relevance here is the
shared dirty-tracking primitive. Not a dependency for the first cut.

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
   `events_cursor`.
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
- Prefer rung-1 only when the rolled-back window is small; a tunable
  checkpoint cadence (Fix A) bounds `Δ`, so the worst-case rewind the
  user ever sees is one checkpoint interval.

### Fix C — drain hosts before the MIG deletes them

Two layers, defense in depth:

- **Deploy tooling (`engrams-internal`):** before `tf-apply` rolls the
  FC-host MIG, call `POST /api/admin/hosts/:id/drain` for each
  outgoing instance and wait for its sessions to reach a terminal
  evac state (or a timeout), so sessions relocate to a peer *while
  both hosts are alive* (the path ADR 0018 proved works in ~24s).
- **Host-agent SIGTERM (this repo):** ensure the Phase-7 checkpoint
  *records snapshots in coord/PG* (via Fix A's host-owned record +
  reconcile), and lengthen the GCE shutdown grace to cover a realistic
  snapshot+upload, so an *ungraceful* termination still leaves
  recoverable, recorded state.

Reliability-first ([reliability_and_latency_first]): the deploy must
not be able to silently eat a live session. Draining-before-delete is
the prevention; Fix A/B are the safety net when prevention is bypassed
(crash, preemption, partition).

## Consequences

- Active/idle sessions survive coord rolls and FC-host MIG rolls with a
  **warm** recovery to a recent coherent checkpoint (rung 1) — bounded
  work-loss, agent context preserved — falling back to cold-boot
  (rung 2) only when no checkpoint is usable. The ADR 0018 promise
  ("a host dies, the session keeps working") extends to the
  deploy-roll + interrupted-eviction case, and *keeps memory* in the
  common case.
- Periodic checkpointing adds a brief recurring guest **pause** per
  sandbox (full-memory capture on stock FC). Cadence is a tunable
  trade (coarse minutes to start); the dirty-page-tracking path
  (ADR 0022 Option B) is the lever to tighten it without the pause
  cost. Measure pause overhead before tightening.
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
- Cross-repo coordination for Fix C (this repo + `engrams-internal`),
  so Fix C ships behind the deploy change; Fixes A + B are
  independently shippable here.
- New prod validation matrix (mirroring ADR 0018's): coord roll during
  eviction; FC-host MIG roll of an idle session recovering to rung 1
  (warm, memory intact, disk rewound to checkpoint); rung-2 cold-boot
  fidelity (on-disk md5 identical, memory-loss expected); checkpoint
  pause-overhead measurement.

## Status / commit chain

To be filled as commits land (per the ADR-bookend norm):

- [ ] **A** — host-driven periodic coherent checkpoint + host-owned
      durable record + heartbeat re-advertise + coord reconciler
      (subsumes interrupted-eviction recovery).
- [ ] **A.log** — `events_cursor` on the checkpoint; rung-1 rewind
      tombstones post-cursor `session_events` (recovery epoch) + resets
      the live head; transcript/SSE rewind boundary + surviving-side-
      effect surfacing; resumed agent is told it recovered.
- [ ] **B** — `restore_disk_only_for_session` cold-boot primitive +
      `evacuate_dead_source` rung-2 dispatch + fail-fast classification
      in `evac_resumer`.
- [ ] **C** — drain-before-roll in `engrams-internal` deploy tooling +
      host SIGTERM checkpoint records-to-PG + grace-period bump.
