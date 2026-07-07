# 0079 — Durable per-session op log with fencing epochs: the lifecycle kernel

Status: Proposed (2026-07-07)

Issue: #543 (2026-07 core-ops overhaul, Tier 2, the epic that retires the
most code). Depends on #526 (telemetry), #529 (host-durable eviction
finalize — the evict verb's finalize step consumes its on-disk record),
#530 (generation purge). Builds on the landed train: ADR 0073 (binding
epoch + durable outbox, #542), ADR 0074 (parking ladder, #545/#581/#585),
ADR 0077 (transactional snapshots + durable_head, #544), ADR 0078
(GCS-free resume, #548/#586/#587).

## Problem

Every session lifecycle verb (create-boot, resume, evict, deliver,
destroy, checkpoint-finalize) is an imperative multi-step async pipeline
inside one coordinator task, serialized by a wall-clock PG lease
(`session_lease`, 180 s reaper) and guarded by a patch ecosystem grown
one incident at a time: spawn-detach-and-move-the-guard, per-write CAS
(`rebind_session_guarded`), two copies of an 8×3 s lease-acquire retry
loop, a 250 ms×8 s Evicting hold, a 500 ms×60 s mid-move delivery hold,
a lease heartbeat whose only job is to out-run the lease reaper, and a
residual-sandbox destroy compensating for crash-between-steps. Users see
follow-up prompts stalling 3–21 s behind a just-finished eviction's
finalize upload, resumes that never complete (16% = 7/45 over 14 d,
2026-07-01 evidence pass), and SendPrompt latency synchronous with an
entire resume. The same three bug shapes (cancelled wire-lifetime
futures, unfenced writes, ad-hoc lease bugs) recur across every
subsystem; ~30% of all commits since May are `fix:`.

## Decision

One primitive replaces the ecosystem. Every lifecycle verb becomes a
durable `session_ops` PG row driven by a single-writer-per-session
executor with a **fencing epoch** CAS-bumped at claim and stamped into
every PG write and every session-scoped host RPC. Wire handlers only
enqueue and observe.

**Invariant made structural:** at most one lifecycle operation executes
per session at a time, in order; every write it makes — PG or host RPC —
is provably fenced by the op's epoch; no lifecycle side effect is ever
owned by a wire-lifetime future; a crashed executor's successor resumes
at the last durable step.

### Schema (migration `0092_session_ops.sql`; high-water at authoring = 0091)

The issue's DDL verbatim, renumbered: `session_ops(id BIGSERIAL, session_id
FK CASCADE, kind, payload JSONB, state queued|running|done|failed|cancelled,
step, epoch, attempts, not_before, idempotency_key, claimed_by, claimed_at,
heartbeat_at, error, created_at, finished_at)` with the
`session_ops_one_running` partial unique index (one running op per session,
enforced by the database — the PG-leasing-row convention at op
granularity), the idempotency partial unique, the queued-head index, and
the running-heartbeat index. Plus `sessions.current_epoch BIGINT NOT NULL
DEFAULT 0` — the fence. Claiming an op CAS-bumps `current_epoch` and
stamps the op row in one transaction; a racing second claimer fails on
the partial unique index; `FOR UPDATE SKIP LOCKED` keeps the race cheap.

**`current_epoch` vs `binding_epoch` (deliberate, distinct, coexisting):**
`sessions.current_epoch` (this ADR) fences *which op executor may act on
a session's lifecycle*, bumped once per op claim. `sessions.binding_epoch`
(ADR 0073) fences *which harness attach is current*, bumped on sandbox
reassignment. An op (e.g. resume) may bump `binding_epoch` as one of its
steps; the two never unify.

### Fenced writes — uniform CAS, not CAS deletion

- **PG**: a `MetadataStore` `fenced_*` family appends
  `AND current_epoch = $e` to every session-row write an op makes
  (`fenced_transition`, `fenced_assign_sandbox`, …). 0 rows affected ⇒
  the executor was fenced by a successor's re-claim; it stops silently —
  never retries, never compensates. `rebind_session_guarded`'s bespoke
  expected-state list collapses into this one predicate.
- **Host RPCs**: `uint64 fencing_epoch` on every session-scoped request
  in `host_service.proto`. Host-side: one `check_session_epoch` helper in
  `grpc_server.rs`, called right after `check_wire_version` (the same
  transport-level-gate move). The host persists a per-session monotonic
  high-water epoch in the sandbox dir (`sandbox.json` — host-durable
  across host-agent restarts, the file ADR 0073 already extends): reject
  `incoming < stored` with `FAILED_PRECONDITION`, store `max` on accept.
  `fencing_epoch = 0` is rejected on migrated session-scoped RPCs — no
  silent-compat path (zero users, clean break). WIRE_VERSION 11 → 12,
  lockstep coordinator+host roll.

### The executor (`crates/engram-coordinator/src/session_ops.rs`)

- **LISTEN/NOTIFY-hot (hard requirement)**: `enqueue_op` inserts the row
  and `pg_notify('session_ops', session_id)`; `pg_listener.rs` gains the
  channel; a 5 s fallback poll covers missed notifies only. Enqueue→claim
  must be milliseconds — a poll-hop executor is a rejected outcome.
- **Inline same-transaction claim**: `enqueue_and_claim` inserts AND
  claims in one transaction when nothing is running for the session —
  the idle-session happy path is one PG round trip, strictly cheaper
  than today's lease-acquire-plus-retry.
- **Completion re-drive**: finishing an op immediately attempts the
  session's next queued op.
- **Step durability**: verbs are explicit step sequences;
  `record_op_step(op_id, epoch, step)` is itself fenced. Step handlers
  are idempotent-from-step.
- **Crash recovery**: a reclaim sweep inside the executor finds
  `running` rows with stale `heartbeat_at`, re-claims by CAS-bumping
  `current_epoch` (the old writer is fenced everywhere), and resumes at
  `step`. Fence-then-resume — never the lease reaper's
  free-the-lock-and-hope.
- **Retry on the row**: failure requeues with `not_before = now() +
  backoff(attempts)` or goes terminal `failed` per verb classification.
- **Cancellation**: `queued → cancelled` is a fenced UPDATE; running ops
  check a cancel flag between steps. The user-visible cancel(evict)
  ascent stays ADR 0074's (already shipped as rung 1/2).

### Verbs in this PR (big-bang: issue phases 1–3 in one change)

- **resume**: `ensure_active` enqueues-and-claims; steps
  `pick_snapshot → restore → bind → finish`. Deletes both 8×3 s lease
  retry loops, the resume spawn-detach + heartbeat, the Evicting hold
  (an arriving resume queues behind the in-flight evict op — ordering by
  log, not poll), and the residual-sandbox destroy (crash-between-steps
  resumes at the recorded step; the residual case is unrepresentable).
- **evict**: nomination enqueues; steps `pause_capture → finalize_upload
  → commit_row → destroy → mark_idle`, the finalize step consuming ADR
  0069's host-durable record. The ADR 0074 rung-2 park is the
  `pause_capture` step's park arm (park ⇒ op completes early at
  `parked`); the rung-2 ascent cancels a queued evict op or, mid-run,
  sets the cancel flag.
- **deliver**: the outbox driver's per-session single-flight becomes the
  deliver verb — ordered behind any in-flight resume/evict on the same
  session. The outbox row, 202 framing, and ack wiring stay ADR 0073's.
- **create_boot**: the queue scanner's boot drive becomes ops (attempt
  budget + `not_before` backoff replace requeue-by-poll); the scanner
  demotes to enqueue-on-capacity.
- **destroy**: `DELETE /session` enqueues; `snapshot_core`'s
  spawn-detach dies.

### What gets deleted (same PR)

The issue's table, re-anchored post-train: the `SessionLeaseGuard`
ecosystem + heartbeat + reaper (idle_evictor.rs), the `MetadataStore`
lease methods + `session_lease` table (dropped by `0093`), both lease
retry loops, `ensure_active_after_evicting_hold{,_for}` + its polls, the
spawn-detach pipelines and their heartbeats, the residual-sandbox
destroy, `rebind_session_guarded`, the eviction scanner's inline
pipeline drive, the queue scanner's requeue-by-poll, and (if the evict
verb no longer awaits it) the `SnapshotWait` shared-future machinery.

NOT deleted: `dead_host.rs` / `reconcile.rs` / `preemption_drain.rs`
(fleet-scoped detectors — their session writes become fenced enqueues),
`enable_scanner.rs` (#546), the GC sweepers (ADR 0077 phase 6), the
idle detector (it nominates; nomination now enqueues).

## Non-goals

- No new `Resuming` FSM state — in-flight visibility is the op row.
- The 24% eviction-snapshot-loss class is ADR 0069's; the evict verb
  only absorbs its shape (finalize as a step-recorded op).
- The delivery outbox schema stays ADR 0073's.
- `commit_snapshot`/`abort_snapshot` removal + `durable_head` semantics
  stay ADR 0077's.
- **Phase 5 (CaptureSpec/RestoreSpec seam consolidation) is deferred to
  a follow-up PR** despite its precondition (ADR 0077's blobs-first
  commit) having landed: it reshapes the `SandboxBackend` seam across
  FC/VZ/Process and the issue itself calls it the epic's
  highest-risk/lowest-value slice; the kernel does not depend on it.

## Testing

Live-PG: claim CAS + one-running index; fenced-write 0-row; step-resume
after reclaim; ordering (evict→resume strictly ordered, zero sleeps);
idempotency-key no-op; cancellation. Host-agent: `check_session_epoch`
accept-monotonic / reject-stale / high-water persistence across restart,
beside the wire_version tests. Existing scanner tests migrate (not
delete) to op-verb equivalents. e2e stack keeps
create→prompt→evict→resume→prompt through the change. `.sqlx/` is not
used for runtime queries (sqlx::query, unprepared) but regenerate if any
macro query lands. WIRE_VERSION 12 lockstep roll at deploy.

## Divergence log

(updated during implementation)

### Core verb migration (resume + evict + lease retirement)

- **Two extra `OpKind`s for inline claims** (`checkpoint_finalize`,
  `teleport` — both anticipated by 0092's header comment). Pipelines
  that are NOT verbs yet but still need the per-session exclusion (the
  manual snapshot, the evac resume, the live teleport) hold an
  `OpClaim` — an op-log row claimed inline (claim-or-give-up; a queued
  row is withdrawn by id via the new `op_cancel_by_id`), driven by the
  holder, finished explicitly. If the holder dies, the reclaim sweep
  re-claims the row and the verb registry terminally FAILS it
  (fence-then-free) — these pipelines are not step-re-drivable yet;
  their existing recovery machinery (periodic checkpoints, the
  parachute/evac scanner) owns the rest. One primitive, no parallel
  lease. The migration's `touch_checked` loop became
  `OpClaim::touch("finalize")` (Held/Lost/TransientError mapping
  preserved) at a 20 s cadence (< the 60 s reclaim staleness).
- **Resume verb status map**: `Active → Done` (idempotent — also what
  terminates a resume op queued behind a rung-2 ascent);
  `Evicting → Retry` (ordered behind the evict op, as designed);
  `Evacuating → Failed` (NOT Retry: a retrying resume op would sit
  ahead of the evac scanner's own claim in the queue and starve the
  very relocation that would resolve it — the wire caller still sees
  the retryable "relocating" 409); terminal/`Dead` failures carry a
  `gone:` prefix the bounded observe maps back to a 410.
- **`pick_snapshot` step dropped**: the walk over snapshot records is
  cheap and re-runnable; steps are `dispatch → restore → bind → finish`.
  The crash-resume shortcut (step ≥ `bind` + a live binding → skip to
  finish) replaces the deleted residual-sandbox destroy; a restore
  whose bind never committed leaves an unreferenced VM for the host's
  ownership-oracle orphan reap.
- **`rebind_session_guarded` NOT collapsed yet**: terminate
  (`DELETE /sessions/:id`) doesn't ride the op log until the destroy
  verb lands, so it flips rows terminal without bumping
  `current_epoch` — the epoch predicate alone would re-bind onto a
  Completed row (#211). The state-list guard stays until then.
- **Evict entry guards by flavor**: nominated ops (detector, scanner
  backstop, rung descent) require `status == Evicting` at claim —
  a rung-1/2 ascent's `Active` flip is thereby an authoritative
  cancel with no lease; admin/drain ops keep the ADR 0044 K5
  `Active | Evicting` set. The rung-descent un-pause moved INTO the
  pipeline (idempotent-from-step) — the reaper only enqueues
  (`allow_park = false`, keyed `evict-descend:{parked_at}`).
- **Evict cancellation is honored only before the capture** (the
  rung-1 window): a post-capture cancel would strand host-side
  finalize state, and the returning user is already served by the
  resume-behind-evict ordering. During the D5 finalize watch the op
  ignores the flag entirely (the session is already Idle).
- **Evict retry budget moved onto the op row** (`attempts`, bumped at
  claim; `EVICT_MAX_ATTEMPTS = 20` → HostLost fallback for nominated
  ops) — the scanner's `max_attempts`/`bump_evict_attempts` machinery
  is gone; the eviction scanner is now a pure re-enqueue backstop
  (idempotency key `evict:{nomination last_active_at}`).
- **Admin evacuate / drain drive the verb pipeline inline** under an
  `OpClaim` (`OpClaim::as_ctx()`), preserving their synchronous
  response shape while staying step-recorded and reclaim-re-drivable.
  `EvictIdle` and `/local` enqueue + bounded-observe; `/local` now runs
  the FULL eviction (fresh capture) instead of the legacy
  destroy-without-capture body — strictly safer; its "a snapshot must
  exist" precondition is kept for endpoint-contract stability.
- **The queue scanner lost its lease with no op replacement for
  creates**: `place_queued_session`'s durable `queued → pending` flip
  is already the single-winner CAS (the create_boot verb is a later
  phase); resume-origin dequeues enqueue the resume op.
- **Live-PG suite hygiene**: the fenced-transition test now flips
  `Pending → Created` (not `Queued`) — a fenced flip to `queued`
  leaves a row with NULL `queue_origin` that poisons the
  queue_scanner suite sharing the database.

### Second verb pass (deliver + create_boot + destroy, fencing posture)

- **Deliver ordering is due-gating, not row reordering**: the deliver
  verb on a non-Active session enqueues a Resume op (payload
  `{"flavor":"for_delivery"}`) and requeues itself — its LOW row id
  stays, but `op_claim_head`'s `not_before <= now()` filter runs
  before the `ORDER BY id` head pick, so the due resume claims first
  and the deliver's retry lands after it (verified store-level by
  `backoff_gated_low_id_yields_the_head_to_a_due_later_op`). The
  outbox row is deliberately NOT deferred on that arm so the retry
  forwards immediately; on a forward failure the row IS deferred
  (`failure_backoff`, unchanged ADR 0073 semantics) — the row stays
  the durable redelivery state, the op is the transient vehicle, and
  the demoted `outbox_delivery` shim re-enqueues a Deliver op per due
  session per wake (deduped by the new `op_pending_exists`).
- **The deliver verb ascends ADR 0074 rungs 1/2 inline** instead of
  enqueueing a resume for an Evicting session: holding the one-running
  slot proves no evict pipeline is mid-capture, so `Evicting` under
  the deliver op is exactly the nomination window or a parked-paused
  VM — `ascend_evicting_to_active` (extracted from
  `try_cancel_nominated_eviction`, which now probes for a FOREIGN
  running op and calls the same core unfenced) cancels queued evicts,
  un-pauses under the op's REAL fence, and flips Active. This is the
  #585 40s-regression fix-forward: a prompt at a parked session
  delivers on the un-paused VM instead of riding a full evict+resume.
- **create_boot retry semantics changed shape**: the retired
  requeue-by-poll bounced a failed boot `pending → queued` for
  RE-PLACEMENT (possibly a different host); the op row retries the
  boot on the SAME reserved host (`attempts`/`not_before`), with a
  30-attempt budget → terminal Failed (+ a `queue_timeout` event) —
  comparable wall-clock to the old 30-min queue timeout. The boot
  `JoinSet` + `boot_concurrency` bound died with it (ops fan out
  per-session); `requeue_session`/`requeue_stale_pending` are deleted
  from the trait + PG (reclaim sweep = crash recovery). A boot that
  crashed past `created` re-drives to terminal Failed (mirrors
  `BootError::Started`), not a re-restore.
- **destroy verb**: `delete_session_core` = enqueue (idempotency key
  `"destroy"` — one destroy per session ever; the burned key makes a
  repeat DELETE observe the terminal row) + a 30s bounded observe that
  settles on the TERMINAL FLIP (the finalize teardown continues behind
  it). The verb replicates `terminate_session` via `terminal_target` +
  the fenced transition; the #211 guarded binding-clear collapsed into
  `fenced_assign_sandbox` under the op's own epoch (also dropping the
  terminal row's dead-weight host affinity).
- **`rebind_session_guarded` NOT deleted (revisited, kept)**: the
  original #211 interleaving (DELETE races resume) IS closed — a
  destroy op queues behind the running resume and its claim bumps
  `current_epoch` — but the fleet detectors still flip rows terminal
  WITHOUT an epoch bump: `dead_host` can drive an Idle-on-a-dead-host
  row `HostLost → Dead` mid-resume-op when every snapshot row's
  `recoverable` flag is stale-false while artifacts survive (the
  promote-failed edge in `snapshot_core`), and the epoch predicate
  alone would bind the fresh VM onto the Dead row. The state-list
  guard stays (both call sites: resume bind, teleport rebind) until
  the detectors' session writes become fenced enqueues.
- **Epoch-0 reject is DEFERRED** (the ADR's "0 rejected on migrated
  RPCs" claim is narrowed): allow-but-don't-advance stays host-side.
  Post-pass-2 the remaining epoch-0 senders are all deliberate and
  perform no epoch-protected lifecycle writes: the direct
  (capacity-available) create pipeline, the wire-path rung ascent
  (`try_cancel_nominated_eviction`), `preemption_drain`'s shutdown
  destroys, and the admin in-place pause/resume debug endpoints. The
  disposition table lives on `check_session_epoch` in the host-agent;
  the reject flips when those paths migrate.
- **Fences threaded to the host on every op path**: the resume verb's
  `restore_for_session`, the deliver verb's `reattach_harness_in_place`
  + rung-2 un-pause, `evacuate_dead_source` (evac-resumer claim /
  resume verb's disk-only path), `boot_on_reserved_host` (create_boot
  verb; unfenced from the direct create), and all four teleport-claim
  RPC sites in `live_migration.rs`.
- **`send_prompt_core`/`answer_question_core` enqueue the Deliver op
  directly** after the outbox row (no wake hop for first delivery);
  the shim's NOTIFY/2s-poll loop owns redelivery re-enqueues. The 60s
  mid-move HOLD was already gone after pass 1; its comment residue is
  cleaned up.

### Pre-merge review fixes (3 independent review agents)

A pre-merge correctness pass turned up 14 findings; the fixes (all
regression-tested) reshaped several of the postures above.

- **(#1) Within-step heartbeat + `RECLAIM_STALE` 60s → 180s.** Heartbeat
  was stamped only at `ctx.step()` boundaries, but a step brackets a
  multi-second-to-minute host RPC (the ~92s prod GCS-page-in restore,
  composed capture/upload, boot), so `RECLAIM_STALE = 60s` re-claimed a
  LIVE executor mid-RPC → a duplicate concurrent pipeline on one VM. Fix:
  a background `OpHeartbeat` per running verb (`spawn_op_heartbeat`, 15s)
  stamps a NEW `op_heartbeat` (bumps `heartbeat_at` ONLY — never the step
  marker) for the whole body, so liveness is proven independently of step
  progress; `RECLAIM_STALE` raised to 180s (≈2× the 92s tail with ~12
  missed beats of slack). The reclaim sweep now fires ONLY on a genuinely
  dead executor — a live-but-slow one is never reclaimed. The proactive
  host-epoch-advance at reclaim time (originally proposed 1c) is therefore
  **deferred as unnecessary**: a dead executor issues no more RPCs, and
  the successor's first fenced host RPC advances the host high-water
  regardless. A resume-verb attempts budget (`RESUME_MAX_ATTEMPTS = 60`)
  terminates any residual livelock (`gone:` after ~an hour of continuous
  failure).
- **(#2) Wire-path rung ascent now runs under a real `OpClaim`.**
  `try_cancel_nominated_eviction`'s old `op_running_for == None` probe →
  unfenced `ascend_evicting_to_active` had a probe→ascend window where an
  evict op could inline-claim (park-reaper descent, scanner backstop,
  fresh nomination) and race the resurrection (Idle→Active with a NULL
  sandbox → the deliver Active-arm loops "no live sandbox" forever, or an
  un-pause mid-capture). Fix: acquire a short-lived `OpClaim(Resume)` (via
  the atomic exclusive claim, #8) and run the ascent under its REAL fence
  — mutually exclusive with any evict op via `session_ops_one_running`; a
  busy lane returns the same retryable "not ascended" the caller already
  handles. `Resume` as the claim kind keeps a holder-death re-drivable by
  the reclaim sweep.
- **(#3) `fenced_transition_session` reads `current_epoch` in the same
  `SELECT … FOR UPDATE` and returns `Ok(None)` on epoch-mismatch BEFORE
  the `try_transition_to` legality check.** Previously legality ran first,
  so a fenced executor whose successor already transitioned got a bare
  `Conflict` (no `fenced:` prefix) → callers matching
  `starts_with("fenced:")` fell to the generic Err arm and fired
  `abort_inflight_snapshot` — the 89f7984d brick compensation a fenced
  executor MUST NOT run. Epoch-staleness is now always the silent-stop
  path regardless of the successor's resulting state.
- **(#4) `session_ops_idem` scoped to active states.** Migration 0092's
  idempotency partial unique became `WHERE idempotency_key IS NOT NULL AND
  state IN ('queued','running')` (edited IN PLACE — pre-merge, unapplied
  to prod; local/CI DBs re-migrate from a wiped schema), and
  `op_enqueue_tx`'s `ON CONFLICT` names the same predicate. A TERMINAL
  keyed row (`evict:{last_active_at}`, `evict-descend:{parked_at}`) no
  longer burns the key forever → the scanner/reaper can re-enqueue after a
  terminal failure instead of getting `Duplicate` every tick on a wedged
  Evicting session. The `InMemoryOpLog` mock idempotency check was
  state-scoped to match.
- **(#5) Pending-orphan backstop in the reclaim sweep.** The
  `queued → pending` flip and the create_boot enqueue are two writes; a
  crash between them — OR a terminal-Failed create_boot whose fenced
  Failed flip also errored — left the session permanently Pending with
  nothing to drive it (the deleted `requeue_stale_pending` was the old
  backstop). Chosen over the "atomic flip" (preferred in the review)
  because the failed-flip case needs a backstop REGARDLESS, so one
  mechanism is simpler: `orphaned_pending_sessions(older_than)` surfaces
  Pending rows with no active create_boot op past a 120s grace, and the
  sweep re-enqueues create_boot (the verb re-reads `host_id` from the
  row).
- **(#6) Fenced lifecycle emits + fenced `set_session_park_rung`.** A
  reclaimed-out predecessor could append a stale StatusChanged/Evicted
  AFTER the successor's newer events (event-log tail corruption misleading
  SSE/idle-detect/transcript), and its `set_session_park_rung(0)`
  compensation could wipe the successor's fresh `park_rung = 2` (the #585
  stall reborn). Fix: `emit_fenced` (append-if-`current_epoch`-current, in
  the same idx-allocation UPDATE via `append_session_event_fenced`) now
  carries every op-path lifecycle emit (evict pipeline, D5, park
  bookkeeping, HostLost fallback, create_boot Failed, destroy, resume
  finish, rung ascent); `fenced_set_session_park_rung` (adds
  `AND current_epoch = $e`) carries every op-path park-rung write. The
  out-of-op nomination (`idle_detector` rung 1) keeps the unfenced
  variants. A handful of intermediate resume emits (Idle→Created on the
  disk-only cold-boot leg) stay unfenced — they fire immediately after a
  fenced `ctx.step`, so a fenced predecessor has already stopped.
- **(#7) DEFERRED with justification.** The source-mutating migration
  RPCs (`migration_capture`/`_presetup`/`_capture_postcopy`/`_commit`/
  `_abort`) still carry no `fencing_epoch`. The fix would ripple a
  `SessionFence` param through the proto, 5 `HostClient` impls, the gRPC
  server, and ~11 test mocks — and finding #1's within-step heartbeat
  already closes the TRIGGER: the teleport pipeline holds an `OpClaim`
  whose heartbeat keeps it alive, so a LIVE teleport is never reclaimed;
  the reclaim sweep only fires on a genuinely dead holder (whose in-flight
  migration RPCs aren't coming), and the `Teleport` verb arm terminally
  fails a reclaimed row (`fence-then-free`), with the parachute/evac
  machinery owning recovery. Same posture the review accepted for #1c.
  The proto fields ride the existing v12 bump when this lands.
- **(#8) `op_enqueue_and_claim_exclusive` — atomic claim-or-fail.** The
  inline-claim callers (admin evacuate/drain, evac-resumer, teleport) used
  enqueue-then-`op_cancel_by_id`, leaving a `queued` row the executor
  could claim and run the FULL verb between INSERT-commit and the cancel
  (an unrequested relocation after the caller was told "busy"). The new
  method inserts + claims in one transaction and ROLLS BACK (row included)
  if the lane is busy — no grabbable row is ever left behind. `OpClaim::
  try_acquire` uses it.
- **(#9) Resume verb Evicting arm ascends.** A direct `/resume` on a
  parked-paused (or nomination-window) session — Evicting, evict op
  already Done — hit `Retry`-forever instead of a ms un-pause. Since the
  resume op HOLDS the one-running slot (no evict is capturing), the arm
  now attempts `ascend_evicting_to_active` under its fence (parity with
  the deliver verb) and only falls to the ordered-behind-evict Retry if
  the ascent doesn't land Active.
- **(#10) Resume crash-shortcut is bounded.** step≥bind + Idle + bound →
  finish-only shortcut re-entered forever if the binding was stale
  (evict's `fenced_assign_sandbox(None)` errored, left Idle+bound). After
  `SHORTCUT_MAX_ATTEMPTS = 3` failed finishes it falls through to full
  dispatch (a fresh restore that overwrites the stale binding).
- **(#11) D5 evict op finishes the instant the session is Idle.** The
  inline finalize row-watch held the one-running lane up to 900s, blocking
  a queued resume/deliver (defeating "evict-then-resume collision gone" on
  the full-evict path). The watch is deleted: the finalize is genuinely
  host-owned (lands via the heartbeat reconcile, surviving coord death),
  and a resume in the window falls back to the prior checkpoint (ADR 0028
  — "the same blast radius as an active host death"), the accepted tradeoff
  for making the session resumable the instant it's Idle. Its
  `EVICTION_FINALIZE_ROW_WAIT_TIMEOUT_TOTAL` metric + `ENGRAM_EVICT_
  FINALIZE_*` env knobs are gone.
- **(#12) Deliver probes `op_pending_exists(Resume)`** before its key-less
  resume enqueue, so a stuck session's repeated deliver retries don't
  churn out one Resume row per retry.
- **(#13) `drive_claimed`/`drive_session` de-recursed.** The completion
  re-drive was `Box::pin(drive_session)` calling back into
  `drive_claimed`, nesting one future per queued op on a burst. Split into
  `drive_one` (drives exactly one op, no re-drive) under a single
  iterative `drive_session` loop — O(1) stack depth.
- **(#14) ACCEPTED.** The resume verb no longer surfaces
  `SnapshotResponse.snapshot_id`/`size_bytes` (always `None`) — the wire
  observes the op row now, not a synchronous `SnapshotResponse`, and the
  op-observe path carries no snapshot id. Low-value contract regression;
  left as-is.
