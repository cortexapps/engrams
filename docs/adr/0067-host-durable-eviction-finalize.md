# ADR 0067: Host-durable eviction finalize (+ `paused_at` cursor, kind-scoped rewind)

Status: 2026-07-01 — **Accepted.** Deep-research-synthesis-driven (issue
#529, part of the 2026-07 core-ops overhaul). Authored alongside the
implementation per the team's ADR-bookend norm; the commit chain below
is the OSS delivery. Extends [ADR 0028](0028-eviction-durability-under-host-roll.md)
(periodic checkpoint durability — the `CheckpointRecord` /
heartbeat-advert / PG-reconcile pattern this ADR reuses wholesale for
the eviction-finalize flavor) and [ADR 0045](0045-live-session-teleport.md)'s
Phase D5 (the begin/wait split this ADR rewrites). Does not touch
`commit_snapshot`/`abort_snapshot`/`snapshot_wait`'s trait surface or
the composed (`snapshot()`) pipeline those still serve — full retirement
of that surface is `epic-transactional-snapshots-revive`'s scope,
explicitly staged to build on this ADR without waiting for it.

## Problem

ADR 0045 D5 split idle eviction into a fast pause-side capture
(`snapshot_begin`, synchronous — the session flips to Idle as soon as it
returns) and a background upload (`snapshot_wait`, awaited by a
coordinator-RAM task that only then writes the PG `snapshots` row,
commits, and destroys the sandbox). This closed the *guest-visible
pause* latency problem D5 targeted, but left the finalize's durability
resting on the conjunction of three ephemeral things:

1. a coordinator-RAM `tokio::spawn`ed task (the only intent to write the
   row),
2. a host-RAM `tokio::spawn`ed task + `snapshot_waits` DashMap slot (the
   only driver of the upload), and
3. live gRPC routing from the coordinator to a sandbox that PG already
   says nobody owns (D5 unbinds `sessions.sandbox_id` before the
   finalize is durable).

Any of the three dying — a host-agent pod roll mid-upload, a coordinator
restart, or (found during implementation verification, not in the prior
incident record) the host's own teardown reconcile racing the unbind and
killing the "orphaned" VM its own D5 pipeline just created — silently
dropped the snapshot. The user's next prompt then resumed from the
*prior* periodic checkpoint (up to `ENGRAM_CHECKPOINT_INTERVAL_SECS`,
default 600s, stale) and visibly replayed turns already shown. Prod
evidence (14d window ending 2026-07-01): ~24% of idle evictions (25/104)
hit this; the literal failure signature is `D5 finalize: snapshot upload
failed ... error=sandbox not found`.

A second, unrelated-looking but same-shaped defect: `rewind_session_to_cursor`
tombstoned **every** event kind past the coherence cursor, including the
coordinator's own eviction/resume lifecycle facts
(`evicted`/`status_changed`/`snapshot_taken`/`resumed`) that always trail
it on a clean cycle. 45/45 sampled resumes had `rolled_back > 0` (median
4) even with zero guest-turn loss — the rewind was treating control-plane
bookkeeping as if it were guest memory.

## Decision

**Once `snapshot_begin` returns, the eviction finalize is a host-owned
job that is a pure function of durable on-disk artifacts.** It never
touches the sandbox, the coordinator, or any in-RAM map it didn't just
(re)build from disk. A host-agent process restart re-drives it from
disk; nothing but node loss (NVMe/hostPath loss, confirmed node-durable
per ADR 0044's DaemonSet `hostPath` mount — see Verification below) can
lose it. The coordinator's role shrinks to serialization only: hold the
resume-blocking lease until the row lands (or a deadline passes), never
touch the artifacts.

### 1. `EvictionFinalizeRecord` (`crates/engram-host-agent/src/eviction_finalize.rs`, new module)

Sibling of ADR 0028's `CheckpointRecord`, same write+fsync+rename
persist, at `<checkpoint_dir>/finalize/<snapshot_id>.json`. Carries
`session_id`/`sandbox_id` from `session_bindings` AT CAPTURE TIME (not
re-read from the RAM map on redrive), the FC staging dir, the diff
chain's previous manifest **ref** (content re-fetched from the chunk
store, not the RAM `checkpoint_chains` map), and — when the capture had
a dirty NBD disk tier — the drained chunk bytes, durably persisted to
`dest/disk-pending/<idx>.<hash>` (the one input that otherwise lives
only in host-agent process RAM until an upload consumes it).

`snapshot_begin` (`PooledBackend`, rewritten): gate on
`supports_diff_checkpoints() + checkpoint_dir` (unchanged, VZ/Process
fall through to the composed pipeline via `InvalidSpec`) → idempotent
early-return under a `pending_finalizes: DashMap<SandboxId, SnapshotId>`
(an eviction-scanner retry storm re-observes instead of re-capturing) →
capture → durably persist the disk-pending chunks (if any) → persist the
record → spawn the finalize job, holding the sandbox's capture lock for
the job's whole lifetime → return. No longer inserts into
`snapshot_waits` (that map now serves only `migration_finish_restore`) —
there is nothing left for `destroy()`'s `wait.abort.abort()` to
supersede, which incidentally closes the host's-own-reaper race
mentioned above (it raced `snapshot_waits`; the eviction flavor doesn't
touch it anymore).

The job (`run_eviction_finalize`) runs stage-explicit legs — `Captured →
DiskUploaded → MemoryChunked → BlobsUploaded → terminal` — each
idempotent, each persisted before the next runs, so a crash between any
two legs re-drives from exactly where it left off. Terminal writes the
*ordinary* `CheckpointRecord { kind: EvictionFinal }` (re-advertised on
every heartbeat until the coordinator's reconcile acks it into PG — the
only place the row now lands for this flavor), deletes the finalize
record, and best-effort destroys the sandbox (idempotent — `NotFound` on
redrive, where it's usually already gone, is success). Failures retry
with backoff (30s × attempt, capped 5min) up to
`ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS` (default 10), then quarantine to
`finalize/failed/` — never silent; resume falls back to the prior
periodic checkpoint, the honest checkpoint-interval-bounded floor. **No
claim of "→ 0" loss** — node death still loses NVMe artifacts; that is
explicitly out of scope (staged for `epic-transactional-snapshots-revive`).

`PooledBackend::resume_pending_finalizes` (called at host-agent startup,
alongside the periodic checkpoint driver spawn) loads every un-acked
record and re-drives it. `PooledBackend` gains a weak self-reference
(`set_self_ref`, installed right after `Arc::new(p)`) so the detached job
— built from a small Arc-cloned bundle (`EvictionFinalizer`, the same
minimal-surface-clone pattern as `SnapshotFinisher`) rather than an owned
`Arc<PooledBackend>` threaded through `snapshot_begin`'s `&self` — can
still reach the FULL `destroy()` (egress unregister, NBD slot release,
checkpoint-chain teardown) at its terminal stage.

**Deviation from the issue's original sketch, documented in the module
doc, not silent:** the issue proposed bifurcating the disk leg into a
"live path" (reuse `ChunkedDiskBackend::flush_upload`, preserving its
live-state rebase) and a "redrive path" (reconstruct from persisted
bytes). This implementation always reconstructs from the persisted
`disk-pending/` bytes. The eviction flavor destroys the sandbox
immediately after finalize completes, so nothing ever reads
`flush_upload`'s rebased live state again — the rebase is unobservable
for this flavor. Collapsing to one code path trades a redundant
O(dirty-set) NVMe write+read for meaningfully lower risk (no live
backend handle / flush-pipeline guard pinned across a backgrounded,
potentially long upload window).

### 2. Coordinator: D5 sheds durability authority, keeps only serialization

`finish_eviction_background` (`idle_evictor.rs`) keeps: PG unbind, Idle
flip, `Evicted`/`StatusChanged` emit — unchanged, still ordered
before-destroy per ADR 0016 §A.1.6. The spawned task is replaced by a
row-watcher (~40 lines): hold the `SessionLeaseGuard` (the thing that
serializes a resume against an in-flight finalize — unchanged, a resume
during upload still 409s) via `spawn_heartbeat` (the SAME reap-avoidance
helper the resume pipeline already used — reused here instead of
hand-rolling a second touch loop), poll `get_snapshot(snapshot_id)` every
`ENGRAM_EVICT_FINALIZE_POLL_MS` (default 2000) until the row exists,
release at `ENGRAM_EVICT_FINALIZE_WAIT_SECS` (default 900) if it never
does. No host RPCs, no `record_snapshot`, no `commit_snapshot`, no
`abort_snapshot`, no `destroy` — deleted along with the old task. A
coordinator death mid-watch degrades gracefully: the 180s lease reaper
frees resume regardless, and the row lands via heartbeat whenever the
host lands it — bounded staleness, never loss.

The `SnapshotTaken` event moves to the heartbeat reconcile
(`host_http.rs::heartbeat`): `MetadataStore::record_snapshot` now returns
`inserted: bool` (Postgres `RETURNING (xmax = 0)`), `CheckpointRecord`/
`CheckpointAdvert` gain `kind: Periodic | EvictionFinal` (serde-JSON
additive — no wire-version bump for this piece), and the reconcile emits
`SnapshotTaken` iff `inserted && kind == EvictionFinal` — the row's first
landing, regardless of which coordinator (if any) is up when it lands.

### 3. `paused_at` cursor + kind-scoped rewind

`SnapshotMetadata` gains `paused_at: Option<DateTime<Utc>>` (bincode —
`WIRE_VERSION` 7 → 8, lockstep coord+host roll per existing discipline),
stamped unconditionally in `SnapshotFinisher::finish` from the capture's
exact pause instant. The composed eviction path resolves the
`session_events` coherence cursor from it instead of coordinator
wall-clock `now` sampled after the (possibly multi-second) capture/upload
completes — closing the skew window. (The D5 path needs no equivalent
change: its rows land only via the reconcile, which already uses the
heartbeat advert's `paused_at`.)

`rewind_session_to_cursor` (`engram-postgres`) now excludes
`status_changed`/`snapshot_taken`/`evicted`/`resumed`/
`recovered_from_checkpoint` from the tombstone `UPDATE` — those are
coordinator facts that stay true regardless of what the guest remembers.
A clean evict→resume cycle now tombstones nothing → `rolled_back == 0` →
`apply_rung1_rewind` no-ops → no `recovered_from_checkpoint` emitted.
Guest-derived kinds (`run_*`, `agent_message*`, `tool_call_*`, `exec_*`,
`prompt_*`, `harness_idle`, `user_question`, `question_answered`,
`file_changed`, plus the already-kind-scoped surviving-side-effect
surfacing for `file_shared`/`integration_asset`) still rewind on a
genuine mid-run crash recovery — unchanged.

## Consequences

- Host-agent pod rolls and coordinator restarts mid-eviction-upload no
  longer lose the snapshot — bounded delay (one restart), not loss.
- The host's own teardown reconcile can no longer race-kill an
  in-flight eviction finalize (it never touches `snapshot_waits`).
- A clean evict→resume cycle is observably clean:
  `recovered_from_checkpoint` events and unexplained turn-replay should
  disappear from prod telemetry post-deploy.
- `snapshot_begin`'s latency grows by the O(dirty-set) NVMe write of any
  drained disk chunks (the durability trade, moved off the
  backgrounded-upload path onto the still-synchronous-but-fast local
  write); the guest-visible pause itself is unaffected (unchanged from
  D5's original shape).
- Node death (NVMe/hostPath loss) still loses artifacts captured since
  the last periodic checkpoint — this ADR does not and cannot change
  that; `epic-transactional-snapshots-revive` builds the next layer on
  top of the invariant this ADR establishes.
- New surface: `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS`,
  `ENGRAM_EVICT_FINALIZE_WAIT_SECS`, `ENGRAM_EVICT_FINALIZE_POLL_MS`
  (the last two coordinator-side, the poll interval mainly for test
  determinism); metrics
  `engram_eviction_finalize_{persisted,completed,redriven,quarantined}_total`,
  `engram_eviction_finalize_stage_seconds{stage}`,
  `engram_eviction_finalize_row_wait_timeout_total`.

## Verification

- Confirmed during implementation (an explicitly unverified item in the
  issue): `<work_dir>` — both the checkpoint-records dir and the FC
  snapshot staging dirs — is a `hostPath` (node-durable, `DirectoryOrCreate`)
  volume in the ADR 0044 host-agent DaemonSet
  (`deploy/helm/engram-host-fleet/templates/host-agent.daemonset.yaml`),
  not an `emptyDir` — survives a pod restart on the same node.
- Unit tests (`crates/engram-host-agent/src/pooled_backend.rs`):
  `snapshot_begin` persists the record + is idempotent under a pending
  finalize before returning; the happy path produces a durable
  `CheckpointRecord{kind:EvictionFinal}` and best-effort destroys the
  sandbox; `resume_pending_finalizes` redrives a hand-crafted record with
  the sandbox entirely absent; a terminally-failing finalize quarantines
  and never fabricates a checkpoint row.
- Live-Postgres test (`checkpoint_reconcile_live_pg.rs`): kind-scoped
  rewind tombstones only guest-derived kinds in a mixed span, none in a
  lifecycle-only span.
- Coordinator unit tests (`idle_evictor.rs`): the row-watcher observes a
  host-landed row without the coordinator calling
  `commit_snapshot`/`destroy`; releases its lease at the deadline when no
  row ever lands.
- FC integration test (`crates/engram-host-agent/tests/eviction_finalize_redrive.rs`,
  `#[ignore]`'d, Linux+KVM, wired into `ci.yml`'s firecracker lane): a
  real guest's finalize job is frozen mid-upload (simulating a dead
  host-agent process); an entirely independent second `PooledBackend` +
  `FirecrackerBackend` generation, sharing only the on-disk `work_dir`,
  reattaches the still-live VM (ADR 0044 K2) and re-drives the finalize
  purely from disk; restoring from the result reproduces the planted
  marker byte-identical.
- e2e (`crates/engram-coordinator/tests/e2e_stack.rs`,
  `e2e_resume_preserves_disk_and_memory`, extended): a clean evict→resume
  cycle emits no `recovered_from_checkpoint`.
- Prod validation (post-merge, per the ADR 0028/0034 precedent): the
  `D5 finalize: snapshot upload failed ... sandbox not found` /
  `abort_snapshot failed after pipeline failure` log signatures should
  go to zero over a 7-day post-deploy window (baseline 21 + 14 hits/14d);
  `recovered_from_checkpoint` rate on clean evict→resume cycles should
  drop to ~0 (baseline 45/45, median `rolled_back` 4).
