# ADR 0028: Eviction durability under coord restart + host roll

Status: 2026-06-02 — **Proposed.** Incident-driven. Authored before
code per the team's ADR-bookend norm; the fix lands in three
independent commits (one per failure below) that can ship and roll
separately. Supersedes nothing; extends [ADR 0018](0018-session-evacuation.md)
(evacuation), [ADR 0016](0016-cow-observability-and-continuous-sync.md)
(idle-eviction pipeline ordering), and [ADR 0009](0009-state-reconciliation.md)
(graceful shutdown).

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

A disk-only session *is* recoverable in principle: cold-boot a fresh
guest from the image's base snapshot and attach the recovered disk
(`live_disk_manifest`), accepting in-RAM/process loss. That path
(base-snapshot cold boot, ADR 0020) exists but is not wired into evac.

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

## Decision

Fix all three, smallest-blast-radius first. A, B are in this OSS repo;
C spans this repo **and** `engrams-internal` (deploy tooling).

### Fix A — make eviction crash-consistent via host-owned snapshot records + a reconciler

Principle: the **host** is the durable owner of "a snapshot exists in
GCS," because the host is what wrote it. Coord's PG row is a cache of
that fact, not its origin.

1. **Host persists a durable, self-describing record** of every
   completed snapshot (sandbox_id, session_id, snapshot_id, disk +
   memory manifest refs, blob keys, recoverable) the moment the upload
   finishes — before returning from the `snapshot` RPC — in a location
   that survives the RPC reply being lost (extend the existing
   `sandbox.json` / a sibling `snapshot.json`, already partially done
   by the shutdown path).
2. **Host re-advertises un-acked snapshots** in its heartbeat /
   registration (`rehydrate_sandboxes` already carries a sandbox
   inventory; add completed-snapshot inventory). A coord that restarted
   mid-eviction — or any coord — reconciles these into PG via
   `record_snapshot` (idempotent on `snapshot_id`).
3. **Coord reconciler re-drives interrupted evictions.** On the
   heartbeat path, a session that is still `Active` with a
   sandbox the host reports as *snapshotted-and-paused* (not running)
   is completed through the rest of the pipeline (record → transition →
   destroy) idempotently. This closes the "coord died after
   `host.snapshot()` returned" window.

Net: if the snapshot reached GCS, it becomes a PG row regardless of
which coord pod (if any) survives — *before* any host-deletion can
strand it.

### Fix B — implement disk-only cold-recovery in the evac path

In `evacuate_dead_source`, when `snapshot.is_none()` but a disk
manifest is available, take a **base-snapshot cold-boot + disk-attach**
path instead of a memory restore:

- restore from the session's image **base snapshot** (cold boot, ADR
  0020) with the recovered `live_disk_manifest` attached as the rootfs
  overlay;
- mark the receipt `EvacLoss::Memory { reason: "source-dead-no-snapshot" }`
  (the variant already exists) so the loss is explicit in events/logs;
- on the harness side this is a fresh harness against the recovered
  filesystem — the agent's on-disk work (files, git, pushed PRs)
  survives; in-RAM conversation context does not.

Plus a **fail-fast guard**: if neither a memory snapshot nor a
base-snapshot+disk recovery is possible, route to `Idle`/`Dead`
immediately with a clear reason instead of burning the 20-attempt
budget on `RestoreFailed`. (`NoRecoverableState` already short-circuits;
extend it to "no *restorable* state given what's available.")

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

- Idle/active sessions survive coord rolls and FC-host MIG rolls with
  at most a memory-loss (cold) recovery, never an unrecoverable
  orphan. The user-visible promise of ADR 0018 ("a host dies, the
  session keeps working") extends to the deploy-roll + interrupted-
  eviction case.
- New host→coord surface (snapshot inventory in heartbeat) and a
  reconciler tick; both idempotent, both single-coord-pod safe by the
  same lease/CAS patterns as the existing evac scanners.
- Cross-repo coordination for Fix C (this repo + `engrams-internal`),
  so Fix C ships behind the deploy change; Fixes A + B are
  independently shippable in this repo.
- A new prod validation matrix (mirroring ADR 0018's): coord roll
  during eviction; FC-host MIG roll of an idle session; disk-only
  cold recovery fidelity (on-disk md5 identical, memory-loss expected).

## Status / commit chain

To be filled as commits land (per the ADR-bookend norm):

- [ ] **A** — host-owned snapshot record + heartbeat re-advertise +
      coord reconciler for interrupted evictions.
- [ ] **B** — disk-only base-snapshot cold-recovery in
      `evacuate_dead_source` + fail-fast guard in `evac_resumer`.
- [ ] **C** — drain-before-roll in `engrams-internal` deploy tooling +
      host SIGTERM checkpoint records-to-PG + grace-period bump.
