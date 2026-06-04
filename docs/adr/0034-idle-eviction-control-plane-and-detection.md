# ADR 0034: Idle eviction rides a state machine, not a request future

Status: 2026-06-03 — **Accepted.** CI-validated on PR #71 (full workspace +
live-PG lane + real-FC and e2e-stack jobs green). The remaining
incident-shaped validation runs in prod post-merge: stuck session
`0782bea5` should be backstop-nominated and evicted within the 30-min
hard TTL of the first deploy (`engram_eviction_nominated_total
{source="backstop"}`).

## Context: a session that finished and never went idle

Prod session `0782bea5` (2026-06-03, the engrams-on-engrams dogfood run that
opened PR #69) completed its run cleanly — `run_completed` + `harness_idle`
landed at 05:33:03 — and then sat in `active` for 8+ hours. The host's idle
driver did its job: 30s after `harness_idle` it nominated the sandbox. What
failed sits above it, in two layers.

**Layer 1 — the control plane.** The coord's
`POST /api/hosts/:id/idle-eviction-candidates` handler awaits the full
eviction pipeline (`evict_session_to_state`: lease → snapshot RPC →
record_snapshot → unbind → transition → destroy → commit) **inline in the
axum request future**. The host's POST timeout is 120s; this session's
snapshot (full multi-GiB memory dump + chunked upload) takes longer. At
timeout the host drops the connection, hyper notices, and axum **cancels the
handler future mid-pipeline** — silently. No error log (the future is simply
dropped between awaits), no PG state change, and depending on where the
cancel lands either orphaned snapshot artifacts on the host (cancelled
between the snapshot RPC and `record_snapshot`) or a destroyed-but-Idle
mismatch. The host cleared its in-flight marker and retried on the next
tick: five `idle eviction pipeline started` lines, ~130s apart (120s timeout
+ 10s tick), zero completions, zero errors.

**Layer 3 — detection.** During the fifth attempt's pause/abort churn the
harness vsock connection dropped. The hub's reader-loop exit removes the
sandbox from `connections`, `last_event_at` *and* `last_idle_at`
(`harness.rs`), and `idle_sandboxes()` only nominates **attached** harnesses
— so the running-but-detached sandbox escaped both the soft TTL and the
30-minute hard TTL, forever. Nothing else can catch it: reconcile intersects
PG rows against the host's running-sandbox list, and the sandbox *is*
running, so the world looks consistent; `sessions.last_active_at` only moves
on state transitions, so PG has no activity signal of its own.

(The numbering skips a layer: **Layer 2** is the data plane — eviction is
slow because memory durability is event-driven while disk durability is
continuous. That asymmetry and its fix — host-driven periodic coherent
checkpoints — is ADR 0028 Fix A, deliberately out of scope here. See
"Relation to ADRs 0028 and 0022" below.)

The framing matters: this ADR does **not** make eviction fast. It makes
eviction **correct and observable regardless of how long it takes**, and
makes detection robust to host-side amnesia. A 10-minute eviction should be
a metrics blip, not a stuck session.

## Decision

### L1: `Evicting` is a durable intent marker; a scanner drives the pipeline

A new `SessionState::Evicting` between nomination and completion:

- The candidates handler becomes **fast and pure control-plane**: per
  candidate, if the session is `Active`, transition `Active → Evicting`,
  emit `StatusChanged`, return. Anything else (already Evicting, raced a
  delete, unknown) is an accepted no-op — which is also what absorbs the
  host's 10s re-nomination ticks cheaply. Nothing slow ever runs in the
  request future again.
- A coord **eviction scanner** (same shape as `evac_resumer`, ADR 0018
  commit 12c: 10s tick, list-by-status, atomic attempt bump, retry budget)
  sweeps `status = 'evicting'` rows and drives the *existing*
  `evict_session_to_state(…, Idle)` pipeline to completion. The pipeline is
  unchanged internally — same `SessionLeaseGuard`, registry guard, and
  `abort_inflight_snapshot` cleanup; its terminal transition simply becomes
  `Evicting → Idle` instead of `Active → Idle`.
- `Evicting` is a **pre-pipeline marker, not the pipeline's target**. That
  keeps the pipeline's invariants byte-identical and confines the new state
  to "a durable to-do that survives coord restarts": a coord deploy
  mid-eviction leaves an `Evicting` row that the next pod's scanner picks up
  on its first tick. (This closes the control-plane half of ADR 0028's
  Defect A; the data-plane half — re-recording a snapshot the dead coord
  never wrote to PG — remains Fix A's job.)

**Budget exhaustion falls back to `HostLost`** (20 attempts × 10s tick,
mirroring evac). The alternatives are each wrong: `Active` re-nominates
forever (infinite loop through both detectors); `Idle` lies (Idle means
"snapshotted + destroyed" — after 20 failures there is no durable snapshot
and the sandbox is still running, so a `/resume` would chase a ghost);
`Dead` destroys a healthy, recoverable runtime over a *coord pipeline*
failure. `HostLost` is the honest state — "coord cannot currently reconcile
this session's runtime" — and it is loop-free: HostLost is not Active, so
neither detector re-nominates, and the existing HostLost machinery
(dead-host second stage, host orphan-reap, manual `/resume`) owns recovery.
Budget exhaustion is alarmable (`engram_eviction_budget_exhausted_total`)
and should be ~0.

**Legality-table diff** (`SessionState::can_transition_to`):

```
Active   → … | Evicting                          (new entry edge)
Evicting → Idle        (pipeline success)
         | HostLost    (budget exhausted; host-death sweep)
         | Completed   (user delete mid-eviction)
         | Dead        (defensive: chunks unreferenceable)
```

`Evicting` rows keep their `sandbox_id` (only the pipeline nulls it). If the
host dies mid-eviction, `mark_host_dead_and_orphan_sessions` sweeps the row
to `HostLost` like any other non-terminal session; the racing pipeline's
next PG transition fails legality, `abort_inflight_snapshot` fires, and the
lease releases — clean. A user `DELETE` mid-eviction transitions
`Evicting → Completed` and the racing pipeline aborts the same way.

**API surface while Evicting:** `/exec`, `/prompt`, and `/resume` return the
retryable 409 ("mid-eviction; retry shortly") — the same contract concurrent
prompts already get from the session-lease 409 today, now with an honest
message and without a wasted restore attempt. Explicitly *not* the
`Idle | Evacuating` auto-resume arm: the sandbox may still be live.

**Host side:** the 120s special-case timeout on
`push_idle_eviction_candidates` (ADR 0016 §A.1.5a) is retired — the handler
is fast now, the shared 30s default is ample. The `eviction_inflight`
markers and 180s stale sweep stay as a within-tick guard, but dedup now
comes from the coord (non-Active candidate → accepted no-op).

### L3: a PG-derived detection backstop

The host's `HarnessHub` stays the authoritative, low-latency idle detector
(soft TTL, attached harnesses only — unchanged). The new **backstop scanner**
covers everything the hub can forget: every 60s it queries for sessions
`active` + `sandbox_id IS NOT NULL` whose newest `session_events` row is
older than the hard TTL (default 30 min, env-overridable), and nominates
them into the same `Active → Evicting` lane with a WARN and
`engram_eviction_nominated_total{source="backstop"}`.

Why `session_events` and not a new heartbeat: the events table is already
the durable record of harness activity (every tool call, message, and idle
announcement lands there), it requires no new write path, and "no event for
30 minutes while Active" is precisely the invariant whose violation stranded
`0782bea5`. Hard-TTL-only means it can never fight the host's soft-TTL path
— a busy session always has fresh events. Races with the host's nomination
resolve via `transition_session`'s row lock; the loser's Conflict is
swallowed as a no-op.

This catches the whole *class* — harness detach, host-agent restart with
amnesia, hub bookkeeping bugs — not just the incident's specific hole.

## Relation to ADRs 0028 and 0022

- **ADR 0028 Fix A (periodic coherent checkpoints)** is the L2/data-plane
  complement: it makes the snapshot at eviction time small (delta since the
  last checkpoint) and crash-durable (host-owned record, heartbeat
  re-advertise). This ADR is deliberately layered so Fix A slots in without
  rework: the scanner and `Evicting` lane don't care how long the pipeline
  takes, and the pipeline internals stay untouched here. Land order is
  L1 → L2.
- **ADR 0022 (memory sharing / forking) stays parked.** Option A (File
  backend) is a density play and doesn't touch snapshot cost. Option B's
  online dirty-page tracking would make periodic checkpoints near-free — a
  later accelerant for Fix A, not a correctness requirement. Stock FC diff
  snapshots plus the content-addressed memory chunk store may capture most
  of the same win without the fork; measure when Fix A lands.

## Alternatives considered

- **Detached `tokio::spawn` from the handler** — minimal diff, fixes the
  cancellation, but a coord crash mid-pipeline still loses the eviction
  (ADR 0028 Defect A's control half stays open) and there's no startup
  sweep. The state machine costs one enum variant and one scanner more and
  buys restart-safety.
- **Reusing `Evacuating`** — same machinery, but conflates two semantically
  different flows in every operator view, event stream, and the
  `ensure_active` arms (Evacuating auto-resumes; Evicting must 409).
- **Fixing detection on the host** (track `last_event_at` keyed by running
  sandbox, not connection) — closes only this incident's hole; host-agent
  restarts and future hub bugs stay invisible. The PG backstop subsumes it.
  `idle_sandboxes` deliberately stays attached-only.
- **Bumping the host POST timeout** (120s → 600s) — treats the symptom;
  any sufficiently large VM finds the new cliff, and the pipeline remains
  cancel-unsafe against coord restarts.

## Consequences

- New migration (`0050`): `'evicting'` in the status CHECK,
  `sessions.evict_attempts`, partial index on `status='evicting'`, and a
  `session_events (session_id, created_at DESC)` index for the backstop.
- Two new background tasks on the coord (eviction scanner 10s, backstop
  60s), both unconditional at startup; the startup sweep is the
  coord-deploy recovery story.
- New metrics: `engram_eviction_pipeline_seconds` (explicit buckets to
  180s), `engram_eviction_nominated_total{source}`,
  `engram_eviction_budget_exhausted_total`,
  `engram_eviction_scanner_lag`.
- The web dashboard learns the `evicting` (and hitherto-missing
  `evacuating`) status renderings.
- Weakest edge, accepted: budget-exhausted `Evicting → HostLost` on a
  still-alive host leans on the host's hard-TTL/orphan-reap for sandbox
  teardown and on the alarmed counter for visibility.

## Commit chain

- `SessionState::Evicting` + legality edges + status gates
  (ensure_active / resume 409; delete interplay)
- migration 0050 (`'evicting'` CHECK, `evict_attempts`, partial
  index, `session_events(session_id, created_at DESC)`) +
  `list_evicting_sessions` / `bump_evict_attempts` /
  `list_active_sessions_idle_past` (trait, PG, MiniMeta) + live-PG
  round-trips wired into CI's Postgres lane
- candidates handler → fast Active→Evicting nomination (+ metric
  constants, eviction-pipeline histogram buckets to 300s)
- eviction scanner (evac_resumer shape; budget fallback HostLost;
  startup sweep = deploy recovery)
- L3 PG-derived detection backstop (60s, hard-TTL,
  `ENGRAM_IDLE_BACKSTOP_TTL_SECS`)
- gate tests (prompt/resume/delete during Evicting)
- host: 120s POST override retired; web: `evicting`/`evacuating`
  rendering (◑, ACTIVE-ish bucket — they previously matched no
  filter and vanished from the list)
- fix: engram-postgres row parser learns `'evicting'` (caught by the
  live-PG round-trip)

Validation: workspace `just check`, 135 coordinator lib tests, 26
live-PG tests, and PR #71 CI (incl. the real-Firecracker and
e2e-stack jobs) all green. The dev-vm manual e2e was blocked by a
pre-existing `build_base_snapshot` fc_error on the box (upstream of
any 0034 code path); the incident-shaped end-to-end runs in prod
post-merge instead — the still-stuck session `0782bea5` is the test:
backstop nomination + eviction within one 30-min hard TTL. Prod
watch: pipeline-seconds completions present, nominated{source}
split, budget-exhausted ≈ 0, no Evicting row older than a few
ticks.

## Addendum 2026-06-04 — prod-found durability defect (incident 89f7984d)

The control plane worked as designed; the **snapshot durability** under
it did not. Session `89f7984d` was idle-evicted (correctly, 30s after a
`harness_idle`) but its eviction snapshot came back **unresumable** —
resume died on `read fc manifest.json … No such file or directory`.

Three compounding bugs, none in the state machine itself:

1. **`commit_snapshot` ran after `destroy()`.** The pipeline order was
   `snapshot → record_snapshot → unbind → sandbox_id=NULL → destroy →
   commit_snapshot`. By the time `commit_snapshot` ran, `resolve_owner`
   returned NotFound (the session's `sandbox_id` was already NULL and the
   registry unbound), so it could never clear the host's
   `inflight_snapshots` entry. The failure was logged WARN-only ("let it
   ride — the PG row still references the durable artifacts") — but the
   artifacts were **not** durable: the still-running host's periodic
   checkpoint driver's next `snapshot()` called
   `abort_prior_inflight_snapshot`, which deletes the per-snapshot
   `state.bin`/`sidecar.json` from BlobStorage — while the `snapshots`
   row still said `recoverable = true`. **Fix:** move `commit_snapshot`
   to immediately after `record_snapshot`, before `unbind`/`destroy`,
   while the owner still resolves (`idle_evictor.rs`). Regression guard:
   `evict_idle_session_commits_before_destroy` asserts the call order.

2. **Same defect, deterministically, in the manual snapshot endpoint.**
   `POST /sessions/:id/snapshot` recorded `recoverable = true`, left the
   sandbox running, and **never called `commit_snapshot` at all** — so
   the periodic checkpoint driver was *guaranteed* to abort its artifacts
   within one interval. **Fix:** the endpoint now commits after
   `record_snapshot` (`api/snapshot.rs`). (Dead-source evacuation and the
   SIGTERM/host-roll path are not affected: the former restores from an
   already-durable checkpoint and takes no new snapshot; the latter's
   host exits, so no live periodic driver survives to abort the inflight.)

3. **Resume trusted the stored `recoverable` flag and never fell back.**
   `latest_snapshot_for_session` is `ORDER BY created_at DESC LIMIT 1`
   with no `recoverable` filter, and resume restored from it
   unconditionally — so one bricked row doomed the session even though
   the ADR-0028 periodic checkpoint chain held an intact prior snapshot.
   **Fix:** resume now walks `list_snapshots_for_session` newest-first,
   re-verifies each candidate's backing artifacts (chunk manifests **and**
   the portable `state.bin`/`sidecar` blobs) at the point of use, demotes
   a verified-missing row to `recoverable = false`, and falls back through
   the chain to the disk-only cold boot. This is the durable guarantee —
   it backstops every capture path and self-heals incident `89f7984d`
   from its prior checkpoint on the next resume.

Separately, the upstream trigger (`harness_idle` while the screenshot
showed work in flight) was the in-VM `claude` child exiting mid-turn:
`harness_idle` is emitted only on the child's stdout EOF, and the harness
discarded the exit status, so a crash was indistinguishable from a clean
turn-end. The harness now captures the `ExitStatus`, and an unsolicited
abnormal exit (non-zero / fatal signal — `137` = OOM-kill) is surfaced as
a `System` transcript message, i.e. a durable `session_event` in PG that
survives VM teardown (`engram-harness-claude`).
