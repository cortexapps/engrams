# ADR 0101: Amortized memory durability — epochs, an honest floor, an honest lifecycle

- Status: Accepted (2026-07-21)
- Date: 2026-07-21
- Issues: prod investigation 2026-07-20/21 (this ADR's Context); ADR 0028 (eviction
  durability), ADR 0034 (idle-eviction state machine), ADR 0045 D5 / issue #529
  (host-owned finalize), ADR 0077 (created-park), ADR 0090 (ownership truth +
  quarantine), ADR 0091 (honest lifecycle), ADR 0095 (peer fill)

## Context

Rung-2 durability — disk+memory snapshots for idle eviction and resume — is slow at
every boundary, and the durability floor is weaker than it appears.

**Measured in prod (2026-07-20/21):**

- FC snapshot *create* (pause + diff-snap) is sub-second. The cost is finalize:
  `engram_snapshot_finish_seconds{type=diff}` averaged 25s and 53s on the two fleet
  hosts; one memory diff of 62,090 dirty 512 KiB ranges re-chunked for ~95s. Periodic
  checkpoints (600s cadence) pay this repeatedly; the eviction path pays it again at
  teardown. `engram_upload_budget_wait_seconds` ≈ 0 across ~13k acquisitions — the
  bottleneck is serial structure and per-object round-trips, not bandwidth.
- `evicting` conflates rung-2 park (VM paused in place) with actual descent. Over 7
  days: episodes lasting minutes to hours, several exactly the 28,800s hard TTL, many
  terminated by `host_lost` (a roll caught the eviction), and two sessions livelocked
  in `evicting` (the ADR 0077 × ADR 0090 quarantine-park interplay) while the eviction
  scanner re-found them every 10s.
- Resume: 1.3–3.8s parked/warm-host, 11.7–45.8s cold cross-host. Coordinator overhead
  is milliseconds; the time is host-side restore, and `state.bin` / sidecar / manifests
  are fetched serially.
- The floor: a session flips `idle` once the host fsyncs a **node-local**
  `EvictionFinalizeRecord` (`eviction_finalize.rs` module docs: "nothing but node loss
  can lose it" — i.e. node loss loses it). GCS publication trails in a background
  finalizer. Node loss in that window silently falls back to a ≤10-min-old periodic
  checkpoint.

**Where the time goes (code):**

1. The eviction-final disk leg uploads dirty chunks in a serial loop with checked
   `put_chunk` (HEAD-before-PUT) — while the live NBD flush path is 32-way unchecked.
2. Dirty chunks are written to NVMe `disk-pending/` and read back before upload on the
   common path (a documented simplification of issue #529's bifurcated design).
3. The finalize ladder `Captured → DiskUploaded → MemoryChunked → BlobsUploaded` runs
   strictly serially; `state.bin`, sidecar, and aux bundles upload serially; resume
   downloads them serially.
4. The sparse memory re-chunk (`update_for_dirty_ranges_sparse`) issues a checked
   `put_chunk` per rebuilt 512 KiB chunk: one HEAD + one PUT per chunk, 32-way. At 62k
   ranges that is ~4k round-trip *rounds* — the 95s. Two structural wastes: (a) the
   HEAD always misses for genuinely-new content, and (b) a rebuilt chunk whose hash
   equals the parent manifest's hash needs **no** store I/O at all — it is already
   durable by the parent-manifest-published invariant.
5. Memory dirt accumulates for a full 600s epoch (or since the last evict), so the
   boundary work is O(10 minutes of writes) — and a Full capture is O(guest RAM).

## Decision

Three phases. The unifying idea: **never bring 10 minutes of memory dirt to the
eviction boundary, then make the boundary itself honest.**

### Phase A — remove the serial tail (no semantics change)

- `eviction_finalize::publish_disk_manifest`: bounded-concurrency (32)
  `put_chunk_unchecked`, verifying each returned hash against the recorded hash.
- Hot-path handoff: `snapshot_begin` passes the drained chunk bytes in memory to the
  finalize job it spawns; `disk-pending/` remains write-only on the common path — a
  crash-redrive journal (redrive still reads + re-verifies it). Staging writes fan out
  (bounded) instead of write+fsync one file at a time.
- Overlap the disk and memory legs when redriving from `Captured` (both are idempotent
  publishes); persist the stage bumps in ladder order afterward, so persisted records
  stay byte-compatible and redrive semantics are unchanged. Upload `state.bin`,
  sidecar, and aux bundles concurrently in the blobs leg.
- Sparse re-chunk: skip the store round-trip entirely when the rebuilt hash equals the
  parent's hash (already durable via the published parent manifest); otherwise
  `put_chunk_unchecked` (new-by-construction content — the HEAD always misses);
  concurrency 32 → 96 (the host-global `UploadBudget`, default 96 permits, remains the
  cross-workload arbiter).
- Resume: fetch `state.bin` + sidecar concurrently; fetch the peer pre-pass's memory +
  disk manifests concurrently.

Expected: eviction disk leg from ~O(chunks × RTT) serial to ~seconds; the 95s-class
memory leg to roughly a third of its time (HEAD elimination + same-hash skip + wider
fan-out) — before Phase B shrinks its input.

### Phase B — amortized memory durability (adaptive dirty epochs)

- Drive the checkpoint cadence adaptively — dirty-bytes threshold and
  pressure/cordon/roll signals, ~30–60s typical — instead of the fixed 600s. KVM
  dirty-page tracking is already armed (`track_dirty_pages`); a Diff snapshot already
  writes sparse dirty pages and resets the bitmap.
- Pipeline each epoch's re-chunk → upload as chunks are produced, under the ADR 0091
  per-sandbox capture guard. Eviction then captures only the open epoch: the memory
  leg becomes O(seconds of writes).
- **Invariant: no dirty-bitmap consumption without a durable epoch journal.** The Diff
  consumes the bitmap; the epoch record (ranges covered, chained parent manifest
  version) persists before the capture is treated as collected, and a failed
  collection retries with its ranges still known-dirty (today's chain-poison → Full
  fallback remains the backstop). If pause cost or the snapshot-file workflow becomes
  the limiter, extend the owned FC fork (v1.16, ADR 0045 Phase B) with a narrow
  dirty-bitmap export/ACK-advance endpoint (double-buffered epochs) — a fallback, not
  the first step.
- Full captures (chain seed / chain loss) stay O(RAM) but move off the eviction path
  into their own scheduled operation.
- Side effect: active-session RPO for host loss drops from ≤600s to ~epoch length.

**As implemented (divergences, Phase B PR):**

- The rate signal is the **last epoch's own diff-extent sum** (recorded per sandbox as
  an `EpochPacingSample` in the snapshot post phase) — no new pre-capture dirty-count
  API needed. The controller (`checkpoint::next_epoch_after`, pure + unit-tested) aims
  each epoch at `ENGRAM_CHECKPOINT_TARGET_EPOCH_MB` (256 MiB default) of dirt:
  `next = last_epoch × target / dirty`, clamped to
  `[ENGRAM_CHECKPOINT_MIN_INTERVAL_SECS (30), ENGRAM_CHECKPOINT_INTERVAL_SECS (600)]`.
  No signal (first epoch, Full capture, zero dirt) → the max backstop, so idle
  sessions keep exactly the ADR 0043 economics; only sessions actively dirtying RAM
  earn short epochs.
- The driver got the ADR 0098 `spawn()`/`run_once()` split
  (`checkpoint::run_checkpoint_pass`) it previously lacked; the timer quantum is
  `min_interval`, candidacy is per-sandbox adaptive.
- The bitmap-consumption invariant is satisfied by the **existing** structure — FC's
  Diff writes `memory.diff` before the bitmap is considered consumed, and every
  failure path poisons the chain so the next capture is Full — rather than by a new
  journal format. A separate epoch journal only becomes necessary with the FC-fork
  export endpoint, and lands with it if ever needed.
- Moving Full captures off the eviction path is deferred to Phase C: it requires the
  explicit descent operation (park-until-seeded), which is Phase C machinery.

### Phase C — honest floor + honest lifecycle

- Split `Evicting` per its two real meanings:
  - **`parked`** — VM paused in place, sandbox bound, un-park ~1s. A real
    `SessionState` variant (ADR 0034 discipline: `parse_session_state` every variant,
    exhaustive matches). Park keeps the ADR 0091 capture-lock discipline. Host death
    while `parked` → `host_lost`, never `idle`.
  - **`evicting`** — an explicit, idempotent descent operation keyed
    `(session_id, parked_at)` with bounded attempts and terminal states — replacing
    the 10s re-scan loop that livelocks against ADR 0090 quarantine.
  - **`idle`** — no live sandbox, resumable from a durable snapshot.
- **Flip the floor: `idle` requires the snapshot closure verified in GCS and the PG
  snapshot row present.** Affordable once Phases A+B make the final publish seconds.
  The sandbox binding is not detached before it. This closes the node-loss window: the
  user-visible boundary and the durability boundary become the same event.
- Host dies mid-descent → `host_lost`, and recovery selects the last *published*
  checkpoint — the same fallback as today, but surfaced honestly (ADR 0091
  `CheckpointLag`), never silently.
- The 8h hard TTL becomes an explicit `parked → evicting` nomination instead of a
  state a session sits in.

**As implemented (divergences, Phase C PR):**

- `Parked` is a full `SessionState` variant (`Active → Evicting → Parked` at park;
  `Parked → {Active, Evicting, HostLost, Dead, Completed}`), persisted as `'parked'`
  (migration 0107 CHECK + partial index). `park_rung`/`parked_at` are KEPT as
  ascent/ledger metadata — lifecycle identity moved to the status; retiring the rung
  entirely is a follow-up cleanup, not this PR.
- The floor flip is a fused store operation, `settle_evicted_session_idle(session,
  sandbox, snapshot)`: one guarded UPDATE (status CAS + sandbox match + recoverable-row
  EXISTS) run by the heartbeat reconcile right after `record_snapshot` lands the
  eviction-final row. The evict op still finishes at capture (ADR 0079 finding #11 —
  never hold the op lane for an upload); the session stays honestly `evicting` for the
  seconds-long publication window, and the scanner's bounded attempts fall a
  never-landing row to `HostLost`. New `MetadataStore` surface
  (`list_parked_sessions`, `settle_evicted_session_idle`) carries D4 conformance
  scenarios against both stores.
- Closing the window also closed issue #570 structurally: the sandbox binding now
  survives until the settle, so the teardown reconcile's ownership check keeps
  answering "owned" for a mid-finalize VM even with the capture-in-flight signal
  suppressed — re-pinned by the rewritten cosim twin
  (`suppressed_signal_reconcile_cannot_reap_a_still_bound_capture`).
- The descent transition (`Parked → Evicting`) commits BEFORE the descent op enqueues
  — enqueue-first would recreate the skip-loop half of the ADR 0077×0090 livelock (a
  state-guard Skip terminalizes the op, terminal rows leave the dedup index, fresh op
  forever). A crash between the two self-corrects via the scanner's generic re-enqueue
  (re-parks if pressure abated, descends if it persists).
- Moving Full captures off the eviction path (park-until-chain-seeded) is deferred: it
  needs nothing new structurally now that descent is explicit, and the prod fleet's
  chains are diff-seeded in steady state; revisit if Full-at-evict shows up in the
  time-to-durable-idle histograms.

## Alternatives considered

**Cluster-seal + GCS leak** (replicate dirty chunks to R peer hosts over the ADR 0095
fabric as the durability floor; seal in PG; upload to GCS asynchronously). Rejected:

- It does not touch the dominant cost — chunk *production* precedes replication, so
  teardown stays ~95s until epochs shrink; and once epochs shrink, GCS itself is fast
  enough and strictly more durable.
- R=2 is not a credible floor here: observed failures are *correlated* (drain-gated
  rolls, node-pool replacement — the ADR 0090 addendum records multi-session
  unpublished-tail loss in one operational episode). A believable floor needs ≥3
  replicas across zones, fsync'd SHA-verified receivers (peer-fill's 692 MiB/s was
  measured with neither — it is explicitly a rebuildable cache, not durability), a
  seal/residency schema, replica GC, and a leased leak worker. Large correctness
  surface, bought for a window Phases A+B shrink to seconds.
- Peer-fill remains what it is good at: the resume-latency tier.

**UFFD-WP for continuous dirty tracking.** Deferred: KVM dirty logging already covers
fresh and restored VMs with zero per-write user-space traps; UFFD-WP adds first-write
fault latency to active sessions, O(RAM) re-protection per epoch, and a broader
memory-backend integration. Revisit only if measurement shows pause/extract dominating.

**Writing the UFFD handler's last-resort GCS fetches into the shared cache.**
Rejected: it would break the ADR 0075 single-writer invariant precisely in the window
(host-agent mid-roll) where the writer is unreachable; the fallback is bounded to that
window.

## Verification

- Sim (ADR 0098 D4): new `MetadataStore` surface in Phase C (descent op, floor flip)
  gets engram-sim scenarios against both stores; add a quiescence oracle asserting the
  park × quarantine interplay converges (no unbounded enqueue/skip loops).
- Crash-state (ADR 0099 H5): externally constructed torn epoch journals and per-leg
  finalize records at every byte offset; assert tolerant recovery and the
  no-bitmap-consumption-without-durable-journal invariant.
- FC lane: minimal-size regressions for the parallel finalize and epoch capture, wired
  into `ci.yml`'s `--test` list. No throughput-scale tests in CI.
- Prod: re-measure `snapshot_finish_seconds`, evicting-episode durations, and the
  resume distribution after each phase; new metrics for epoch bytes/age and
  time-to-durable-idle.

## Commit chain

- Phase A: PR #834 (serial-tail removal)
- Phase B: PR #835 (adaptive dirty epochs; + the review fixes: clamp-panic
  order-safety, the detached + per-sandbox-gated dead-guest probe, dead-code
  retirement)
- Phase C: PR #836 (the parked/evicting/idle split + the settle floor; + the
  review fixes: the deliver-verb Parked arm, `ObservedEvict::EvictedSettling`,
  and the key-agnostic grace-bounded scanner suppression)
- Sim follow-ups: PR #837 (the op-mint quiescence oracle — the livelock-class
  pin the 2026-07-21 incident left open — adaptive-candidacy + probe-gate unit
  tests, and the wildcard-match hygiene pass over `SessionState`)

## Production validation (2026-07-21, same-day deploy)

All four PRs auto-rolled to the prod fleet (two coordinator replicas + the
host DaemonSet) the same day. Observed:

- **Legacy drain, clean.** Five pre-Phase-C parked sessions (spelled
  `evicting` + `park_rung=2` under the old model) drained without manual
  intervention: the new scanner minted capture retries (`allow_park: false`,
  their stale terminal ops being far past the settle grace), the hosts
  finalized, and the reconcile settle flipped them `idle`. Three whose hosts
  rolled away mid-drain went honestly `host_lost` and the dead-host straggler
  sweep settled them. At rest: **zero `evicting` rows** — versus the 8-hour
  parked-as-evicting episodes (and the 2.5-day livelock) this ADR opened with.
- **The settle is live**: `"eviction settled Idle: recoverable snapshot row
  landed"` observed on both coordinator replicas — `idle` now factually means
  the recoverable PG row exists.
- **The honest lifecycle is live**: the first fresh nomination post-deploy
  parked in **0.0s of `evicting`** and reads `parked`.
- **Finalize cost**: the drain's own captures — the worst case, carrying hours
  of parked dirt — averaged **~10.6s** against the 25–95s pre-ADR baseline.
  Same-day interim (Phases A+B only), the adaptive cadence was already
  checkpointing busy sessions every ~80s versus the fixed 600s.
- **Pending a stable window**: the `engram_checkpoint_epoch_bytes/_seconds`
  histograms (they emit from each sandbox's second capture on a pod, and the
  fleet churned through the validation) — watch them center near the 256 MiB
  target; if evict-time Full captures show up there, that is the trigger for
  the deferred Full-off-eviction work.

## Incident addendum (2026-07-21): the parked-survivor rollback (session 61a03b7e)

Hours after the Phase C deploy, a routine host-agent roll landed while a
session sat freshly `parked` (rung-2, VM paused in place, memory not yet
settled to the durability floor). The resume rewound **93 events** to a
periodic checkpoint seven minutes old — on a healthy host, with every log line
along the way at WARN or below. Two independent drift bugs plus one
observability gap:

1. **The reserving-states SQL drift (coordinator/postgres).** Phase C added
   `parked` to `SessionState::host_memory_reserving_states()`, but FIVE
   hand-rolled SQL literal twins of that list in `engram-postgres` never
   picked it up — most fatally
   `list_resident_sandboxes_on_host_with_disk_manifest`, so the register-time
   rehydrate list omitted the parked survivor (the exact 731df805/#739 shape,
   one state newer). Also affected: both placement reservation aggregates
   (parked VMs held RAM the packer wasn't counting), the `delete_host` guard,
   and `list_active_sessions`. The sim twins all used the typed predicate, so
   the conformance suite could only have caught it with a parked-at-register
   scenario — which didn't exist. **Fix**: the SQL lists are now interpolated
   from the const (`reserving_states_sql()`), and the conformance suite pins a
   parked session in all four surfaces against both stores.
2. **The local fallback's manifest-kind bug (host-agent).** With the coord
   list empty, the #739 local survivor pass fed the `ChainHeadRecord`'s
   manifest — the MEMORY chain head — into the NBD disk rehydrate. That (a)
   made the predecessor's legitimate shutdown spool look foreign-lineage and
   DISCARDED the only copy of its acked writes, and (b) failed the reattach
   with `ManifestKind` mismatch, quarantining the device. The ADR 0090
   quarantine ladder then found the parked VM "unevictable" (its disk
   unserved, so no capture possible), destroyed it, and settled `HostLost` —
   converting a healthy pause into host-death semantics. **Fix**: the local
   pass now resolves disk lineage from the shutdown-spool marker (meta-only
   read; the only local durable disk-lineage source) and skips — without
   claiming the slot — when there is none; a lineage-mismatched spool is now
   PRESERVED under a `shutdown-spool-lineage-mismatch` soft-invariant +
   counter instead of discarded.
3. **Silence.** The destroy logged one WARN; the 93-event rewind logged INFO.
   **Fix**: destroying a `Parked` quarantined survivor now fires the same
   loud triple as the #829 budget-exhaustion arm (ERROR +
   `engram_durability_rollback_total` + the durable `durability_rollback`
   event), and `apply_rung1_rewind` WARNs and bumps
   `engram_session_rewound_events_total{cause}` — `checkpoint_lag` on healthy
   hosts is the alertable signature this incident had and nothing watched.

The lesson for this ADR's ledger: **a new lifecycle state is a schema change
for every hand-spelled status set in the system.** The typed enum was
wildcard-audited in #837; the SQL literals were not. They now share one
source.
