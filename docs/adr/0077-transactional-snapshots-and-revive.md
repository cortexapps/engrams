# 0077 — Transactional snapshots, durable_head, RuntimeSpec, and Revive

Status: Accepted (2026-07-06)

Landed (this ADR's PR chain): phase 1 (per-session `durable_head`,
advanced in the record-snapshot TX — `aa7394c4`), phase 2 (fork disk
manifest identity at attach not first flush — `dc7219ab`), phase 3
(`RuntimeSpec` persists boot inputs; fixes the queued-skills TODO(P1-D)
— `a87af0d3`), phase 4 coordinator-half (`Idle → Active` edge, no
`Created` limbo — `778f4af0`). Sequenced follow-ups with their gates:
phase 4 host-composite Revive RPC → #549 (new proto surface #549
re-migrates); phase 5 trust-the-row → external prod fleet audit; phase 6
reachability-GC merge → after phase 5 (see the divergence log — the
sweep loop is already consolidated; the residual body-merge is coupled
to phase 5's root change and is the epic's lowest-value/highest-risk
piece).

Issue: #544 (2026-07 core-ops overhaul, Tier 2). Depends on #529
(host-durable eviction finalize — this epic subsumes it), #526/#527
(the SLO canary gates the trust-the-row flip). Builds on ADR 0074
(the parking ladder, whose cancel/ascent edges this epic's Revive extends
with the direct Idle→Active resume edge).

## Problem

A snapshot's durability is split across two commits — blob upload and
the PG row — with a `recoverable` flag bridging them, so a crash
between them leaves a phantom: `recoverable=true` with missing blobs
(the 24% D5 eviction-loss class, #529's territory) or an uploaded
snapshot with no row. Resume then verifies 4 GCS HEADs per candidate
to paper over the gap (200-400ms/resume), re-derives the boot inputs
from scratch every time (the queued-skills TODO, the forge-token
FK-ordering shape, post-resume egress drift), and runs an 8-12-step
pipeline the op-log epic will later fence. GC is four overlapping
sweepers keyed on the same `recoverable` flag.

## Decision (six invariants, phase order)

1. **Row existence == durability.** A commit uploads ALL blobs, then
   ONE PG transaction inserts the `snapshots` row AND advances a
   per-session `durable_head` (newest-by-created_at wins). No
   `recoverable=false` staging rows, no commit/abort RPCs. Either the
   row exists (blobs durable, GC-pinned) or nothing does (host janitor
   reaps the orphan dir; pin-set GC reaps unreferenced blobs).
2. **Disk manifest forks at attach, not first flush** — `fork_pending`
   / `fork_on_first_flush` deleted.
3. **RuntimeSpec** — a session's boot inputs are one persisted
   document (create-transaction, refreshed at finalize), consumed by
   create/resume/queue/evac. Kills the re-derivation class.
4. **Revive** — one epoch-fenced host RPC replaces the resume
   pipeline; coordinator side is one PG transaction (bind + Idle→Active
   + events). Fence epoch is 0 until #543 lands.
5. **Trust the row (GATED)** — drop the 4-HEAD verification and the
   `recoverable` column; select by `durable_head`, falling back
   through the chain ON RESTORE FAILURE. Flip only after (a) a
   one-time fleet audit that every `recoverable=true` snapshot's blobs
   exist, (b) restore-failure fallback tested, (c) the SLO canary
   asserts the evict→resume cycle.
6. **GC = reachability from durable heads** — one sweeper replaces four
   (`bundle_gc` explicitly OUT — its roots are the fleet stamp, not
   snapshot reachability).

## This PR: phase 1

`sessions.durable_head_snapshot_id` (migration 0087) + a
`record_snapshot` that advances the head in its EXISTING transaction
(the row-write + chunk_generation bump already transact), advancing
ONLY when the new row is newer than the current head by `created_at`
(monotonic; a re-record or an out-of-order reconcile never regresses
the head). Base captures (`session_id IS NULL`) skip the head advance
(their head is `enabled_images.base_snapshot_id`). The `recoverable`
column and the resume verification stay for now — phase 5 removes them
behind the gate. Phases 2-6 are follow-ups on this ADR.

## Divergence log

- **Phase 2 forks at v0.** The eager fork mints the private id at
  version 0 (not v1) so the first flush's `next_version()` lands the
  first persisted manifest at v1 — the stored chain starts at v1
  exactly as the pre-0071 fork-on-first-flush did.
- **Phase 3 scope.** RuntimeSpec ships the fields whose re-derivation
  was lossy (selected_skills — the TODO(P1-D) fix) plus harness +
  workdir; the egress template + sealed secret refs are phase 3b (the
  resume path still re-resolves those, which is correct-but-redundant,
  not buggy — so lower priority than the skills drop).
- **Phase 4 (Revive) split: coordinator-half LANDED, host-composite is
  the follow-on.** The Revive win has two halves. (a) The
  coordinator-side state-machine simplification — the `Idle → Active`
  legality edge + removing the `Created` limbo from the resume happy
  path (Idle→Created→Active becomes Idle→Active, one fewer transition +
  event; `Created` stays the start_agent-FAILED resting state for the
  /exec-409 contract) — is DONE here and unit-tested. (b) The host-side
  merge of `restore` + `start_agent` into ONE `Revive` host RPC (the
  ~1-fewer-round-trip half) is deliberately sequenced with #549: it is
  a new proto RPC that #549's one-definition surface would immediately
  re-migrate, so building it on this branch is wasted motion. Its proto
  + coordinator-transaction shape is specified in §Decision 4; the
  fence_epoch stays 0 until #543. The latency arithmetic supports this
  split — resume p50 (12.2s) is page-in + handshake dominated (#534 /
  #548 territory), so Revive's own contribution is a
  reliability/simplicity win, which the coordinator-half delivers.
- **Phase 6 (reachability GC): the honest scope is a 2→1 sweep-body
  merge, coupled to phase 5, and it sequences LAST.** Reading the code
  corrected two claims in the original Decision:
  1. **"Four sweepers → one" over-counts.** `gc_sweep_loop` ALREADY runs
     `chunk_gc` + `snapshot_blob_gc` + `bundle_gc` on ONE tick with one
     shared `ChunkGcConfig` (interval / grace / barrier). `bundle_gc` is
     explicitly OUT (its roots are the fleet stamp). And
     `base_snapshot_retention` + `checkpoint_retention` are NOT blob
     sweepers — they are orthogonal ROW-retention policies
     (`prune_orphan_base_snapshots` / `prune_session_snapshots` delete PG
     rows; the pin-set sweep reclaims the freed blobs on its next tick).
     So the residual consolidation is just merging the two remaining
     blob-sweep BODIES (`chunk_gc` + `snapshot_blob_gc`) into one
     pin-set pass — the loop, config, grace, and barrier are already
     unified.
  2. **It is NOT independent of phase 5.** The merge's value is a single
     root collection, but phase 5 rewrites exactly that root
     (`collect_manifest_refs`: `recoverable` → `durable_head ∪
     retained_chain`). Merging the bodies before phase 5 lands means
     re-touching the merged code when phase 5 flips the roots — wasted
     motion on the most data-loss-critical module in the system.
  So phase 6 sequences AFTER phase 5, as its own PR with dedicated
  live-PG reachability tests + a soak (a reachability bug deletes a live
  session's chunks). Its impact — one fewer sweep body on an
  already-unified loop — is the epic's lowest, which matches sequencing
  it last behind the user-facing latency + correctness work.
- **Phase 5 (trust-the-row) stays externally gated** — the critic's C7
  fleet audit (every recoverable=true snapshot's blobs exist) is a
  prod-vantage action this change cannot perform.

## Re-integration divergence (rebased onto main past #566/#558)

Main landed #566 (create-as-plan, issue #535) and #558 (host-durable
eviction-finalize, #529) BEFORE this epic. Reconciled toward the epic's
design, per the issues:

- **RuntimeSpec subsumes #566's `sessions.selected_skills` column (clean
  break).** #535 explicitly deferred "subsuming this column into
  RuntimeSpec" to this epic, and #566's biggest gap — resume/queue
  re-derivation — is exactly what RuntimeSpec fixes. So phase 3's
  RuntimeSpec is written INSIDE #566's `reserve_and_persist_create`
  transaction (one durable document), the `sessions.selected_skills`
  column is DROPPED (migration 0089), and `reserve_and_persist_create`
  mirrors `runtime_spec.selected_harness` into the pre-existing
  `sessions.harness` column. `prepare_from_row` (queue re-prepare) reads
  skills from the RuntimeSpec. No dual persistence, no dangling column.
- **`record_snapshot` merges #558 + phase 1.** #558 made it return
  `bool` (did-insert, for the SnapshotTaken-once heartbeat reconcile);
  phase 1 advances `durable_head` in the same transaction. Both compose
  in one body — the eviction-finalize path #558 introduced is the caller.
- ADR renumbered 0071 → 0074 (main's 0067-0069 + the overhaul's
  0070-0073 precede it).

## Divergence log (rebase + pre-merge review, 2026-07-06)

Renumbered at land: ADR 0074→0077 (main's 0074 = the parking ladder);
migrations 0087-0089→0088-0090 (main's applied high-water was 0087).
Five review findings were fixed before merge:

- **`get_session` projection**: dropping `selected_skills` from the
  SELECT left a missing comma that aliased
  `live_disk_manifest_version AS park_rung` — every session read as
  un-parked (the rung-2 ascent would flip a frozen VM to Active without
  un-pausing) and `live_disk_manifest` read None (disk-only cold boot
  gated off; post-snapshot flushes silently discarded on resume).
  Fixed in the rebase; `get_session_round_trips_park_and_live_disk_manifest`
  (eviction_live_pg) pins the projection.
- **Fork-at-attach dangling ref**: the phase-2 fork minted an
  UNPUBLISHED `(uuid, v0)` placeholder into `manifest_ref`, which every
  out-of-process consumer treats as store-resolvable — a session
  evicted before its first flush became unevictable (finalize NotFound
  every redrive) and the zero-dirty capture recorded the dangling ref
  into the snapshot row (permanently unresumable). Redesigned:
  `fork_identity` is a separate field the FIRST publish adopts at v1;
  `manifest_ref` stays the resolvable shared base until then.
  `forked_backend_stays_resolvable_until_first_publish` pins it.
- **`durable_head` vs `recoverable`**: the head advanced on
  `recoverable=false` rows (the #213 two-phase capture, the resume
  demote), so an aborted capture could hold the pointer while its blobs
  were deleted — and the monotonic guard blocked older good snapshots
  from reclaiming it. The advance now gates on `recoverable`, and a
  demote that hits the current head re-points it to the newest
  still-recoverable snapshot.
- **RuntimeSpec read errors**: `prepare_from_row` swallowed
  `get_session_runtime_spec` failures into an empty skill list —
  a transient PG blip booted the session's whole life without its
  mounts, silently. Errors now propagate (the scanner/resume retry is
  the recovery).
- **Harness-failed park**: the `Idle→Created` park emitted no
  `StatusChanged` (event-log consumers kept seeing Idle while /exec
  409'd) and swallowed transition failures. It now emits and
  propagates.

Accepted at land (zero users, clean-break): migration 0090 drops
`sessions.selected_skills` with NO backfill into
`session_runtime_specs` — sessions created before this deploy lose
their dynamic skill selection on their next queue re-prepare. Also
noted: the resume/evac cold-boot paths do not yet CONSUME the
RuntimeSpec (only the queue re-prepare does) — the migration comment
overstates; consuming it there is follow-up work, not a regression
(the old column was never read on those paths either).
