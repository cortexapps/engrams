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
discards the exit status, so a crash is today indistinguishable from a
clean turn-end. The most likely cause was the Anthropic bearer token
expiring around midday 2026-06-04 (the same 401 the e2e harness
round-trip then started hitting). Surfacing that exit reason as a durable
event is deferred to the in-flight Claude-harness rework rather than
landing here; this ADR's fix is the durability changes only.

## Addendum (Track A, 2026-06-16): harness-desync watchdog + re-handshake

Incident `bf3dbbcb` exposed a wedge class the two detectors above miss. A
warm-reattached session (ADR 0037) desynced: after a periodic checkpoint it
emitted a bare `agent_message` with **no enclosing `run_started` and no
`run_completed`**, then went silent. The VM was healthy (still
checkpointing, ttyd spawned) — only the coordinator-visible run state was
stuck. The host-side hub never nominated it (the harness was attached and
emitting), and the L3 backstop keys off event *silence* (`MAX(created_at)`),
so it would only reap it 30 min after the last event — and a desync that
keeps *trickling* events defeats it outright. `last_active_at` (state-
transition-only) had frozen at the resume instant, so the operator-facing
liveness signal lied. Root cause is the same inference defect this ADR's
tail already names: the child-per-prompt harness infers run boundaries from
stdout EOF, so a desync/crash is indistinguishable from a clean turn-end.
The proper fix is the streaming-harness rewrite (**ADR 0052**); this Track A
is the server-side safety net that makes the class self-heal regardless.

Three pieces (all PG-derived, so they survive a coord restart and are
operator-visible — no per-pod in-memory counters):

1. **`sessions.last_event_at`** (migration 0068), bumped on every
   `append_session_event` in the same atomic CTE that allocates the event
   idx. An honest activity clock independent of the state machine, and the
   substrate the watchdog ages off.

2. **The desync watchdog** (`desync_watchdog.rs`,
   `MetadataStore::list_active_sessions_desynced`). A scanner that flags two
   *shapes* the silence detector can't see: `orphan_after_close` (the latest
   event is a run-scoped event but no run is open — the `bf3dbbcb` shape) and
   `stuck_open_run` (a `run_started` with zero events since). A healthy
   in-progress run is excluded (its tail pairs to an open `run_started`, and
   `last_event_at` keeps advancing). Detection is metric + WARN only — the
   watchdog never writes to `session_events`, because a coord-emitted marker
   would bump `last_event_at` and mask both its own re-detection and the
   backstop.

3. **Non-destructive recovery via re-handshake.** Because the watchdog only
   fires on sessions with a *live* vsock connection, recovery is an in-band
   `HarnessCommand::Rehandshake` (`HostClient::rehandshake`): the harness
   drops + re-dials and re-emits `Idle` — the SIGUSR1 nudge's twin, but over
   the live command channel, no agentd/guest plumbing. It cannot kill a live
   run, so the watchdog fires on a short 5-min TTL (vs. the backstop's 30).
   Escalation needs no strike column: a working nudge re-emits `Idle`, which
   bumps `last_event_at` and drops the session out of the flagged set; a
   session whose `last_event_at` stays older than the escalate TTL (15 min)
   is where the nudges aren't taking, and it falls through to this ADR's
   proven eviction → resume lane (`eviction_nominated{source=
   desync_watchdog}`).

Commits: `last_event_at` liveness; watchdog detection; in-band re-handshake
chain; recovery + escalation. Metrics: `engram_harness_desync_detected_total`
{signature}, `engram_harness_rehandshake_total`. Pre-rewrite this is the
recovery; post-ADR-0052 the desync rate should fall to ~0 and the watchdog
becomes a pure backstop.

## Addendum (Track A, 2026-06-18): in-place harness reattach before teardown

Prod session `6ed6afdb` exposed a hole between the two recovery rungs above.
Point 3 claims "the watchdog only fires on sessions with a *live* vsock
connection" — but detection is **PG-derived** (`list_active_sessions_desynced`
reads the event shape, not vsock liveness), so it fires regardless of whether
the harness link is up. When the link is **dead** — the harness's established
connection didn't EOF (its blocking read never woke, the reconnect loop never
fired) or the harness process exited and agentd, which only (re)spawns on a
host `SpawnHarness`, sat idle — the in-band `rehandshake` has nothing to nudge:
`HarnessHub::rehandshake` returns `NotAttached`, mapped to `SandboxError::NotFound`
("sandbox not found"). The watchdog logged that every 30 s for **15 minutes**,
burning the full escalate TTL before falling through to the eviction → resume
lane — even though **the FC VM was alive the whole time** (disk daemon flushing
live manifests v17→v22, ttyd up). Tearing a healthy warm VM down to a 2.1 GB
snapshot and restoring it, purely to re-establish a vsock, is the wrong default.

**The missing rung: re-establish the harness IN PLACE, no teardown.** The vsock
transport is harness-dials-host (ADR 0013, NAT-friendly) — the host can't dial
*into* the guest's harness, only the harness can re-dial out. But there is a
second, always-up control surface to the live guest: **agentd** (PID 1), which
supervises the harness. agentd already knows how to revive the link — the
ADR 0045 C1 reattach arm (`harness_supervisor::spawn`) SIGUSR1s a live-but-wedged
harness to drop+re-dial, or reaps-and-respawns an exited one. That arm fires on
a host `SpawnHarness`, which today is sent only at restore time. Our wedge is
that exact failure shape (the C1 comment even names the "prompts 'sandbox not
found' while exec works" canaries) with no restore to trigger it.

So the new rung is just: **re-issue the resume `start_agent` against the
session's existing live sandbox.** `start_agent` is wait-ready (a no-op; agentd
is up) → InstallHostCa (idempotent) → `SpawnHarness` → the C1 reattach/respawn
arm. The spec is the **resume shape** — `resolve_harness(prompt = None)`, the
same builder a real resume uses (now shared as `resolve_resume_agent_and_policy`,
reused by `finish_resume_to_active`). Prompt-less is load-bearing: the initial
prompt rides `req.env`, so a *boot*-shape respawn of an exited harness would
re-inject the original prompt mid-conversation; the resume shape just `--resume`s
the existing claude session and goes `Idle`. A successful reattach re-emits
`Idle`, bumping `last_event_at` and dropping the session out of the flagged set —
identical settle signal to the re-handshake rung.

The recovery ladder becomes three rungs, cheapest first:

1. `rehandshake` — live vsock, event-stream desync. One in-band command.
2. **in-place reattach** (new) — `rehandshake` returned `NotFound` (dead vsock),
   but the VM is alive. Re-issue `start_agent` → agentd SIGUSR1/respawn. Seconds,
   no teardown, warm VM preserved.
3. escalate → `Evicting → Idle → resume` — unchanged backstop, now reached only
   when even the in-place reattach can't settle the session before the escalate
   TTL (agentd genuinely unreachable, VM actually gone). The catch-all that works
   regardless of *why* the link died stays the last resort, not the default.

Coordinator-only change: no new host RPC, wire variant, or agentd code — the
in-place rung reuses `HostClient::start_agent` end-to-end. Best-effort: a
`None` manifest (dev-VM / process backend) or a session that moved off its
sandbox mid-tick is a no-op, never a hard error — escalation still catches a
genuine wedge. Metric: `engram_harness_inplace_reattach_total`. This is the
fast-path that makes "the VM is alive, just reconnect" the common recovery and
relegates teardown to the genuinely-unrecoverable tail.

## Addendum (2026-06-30): pressure-aware soft nomination — evict for *density*, not the clock

**Problem (prod-found on the "Cortex Development"/`dev-brain` profile).** The host's
soft-TTL nomination (`HarnessHub::idle_sandboxes`, L1) fires purely on elapsed idle
time (`ENGRAM_IDLE_TTL_SECS`, 300 s), regardless of whether the host actually needs
the RAM back. For big interactive VMs this is pure loss: a 24 GiB `dev-brain` session
whose user steps away for six minutes gets snapshotted + destroyed, and the next
message pays a cold resume (a 24 GiB working set faulting back in over the substrate
+ pd-ssd chunk tier — measured 14–38 s, up to 92 s) — **even though the host was at
~60 % free RAM the whole time.** Eviction exists for *density* (pack more microVMs per
host by reclaiming idle ones); firing it when there's abundant free memory buys no
density and only adds latency. This is the same interactive-churn failure ADR 0039
follow-up #20 softened by raising the TTL 30 s → 300 s; the TTL is a blunt instrument
for it.

**Change (host-side only; the L1/L3 seam is unchanged).** The eviction driver's soft
nominations become **demand-driven**: a `Soft`-idle candidate is only pushed to the
coord when the host is under real **memory** pressure (free RAM below a floor).
`Hard`-idle candidates (the never-emits-`Idle` backstop) are *always* nominated, and
the coord's own hard-TTL PG backstop (L3, `idle_detect_backstop`, 30 min) is the
absolute residency ceiling either way — so suppressing a soft nomination only defers
reclamation to genuine pressure or the hard TTL, never forever.

- `HarnessHub::idle_sandboxes` now returns an `IdleKind` (`Soft` | `Hard`) per
  candidate; `Hard` wins when both TTLs have fired.
- The eviction tick reads host memory in-process (`util::mem_mib` → `/proc/meminfo`,
  no PG round-trip) and, when the mode is on and free RAM is above the floor, retains
  only the `Hard` candidates. **Memory, not disk:** eviction frees RAM, so memory is
  the correct pressure signal — the pre-existing disk floor
  (`ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES`) stays an orthogonal *brake* on eviction (it
  *writes* a multi-GiB memory dump), not a reclaim trigger.
- **Fails open toward eviction:** if `MemTotal` is unreadable (non-Linux, parse
  failure) the tick behaves as under-pressure, degrading to today's TTL-only behavior
  rather than silently pinning sessions resident.

**Knobs (both default to today's behavior).** `ENGRAM_IDLE_EVICT_PRESSURE_AWARE`
(bool, **default off** — ships dark, per-host flip, instant rollback);
`ENGRAM_IDLE_EVICT_MEM_FLOOR_PCT` (default 15 — reclaim `Soft` candidates only when
free RAM < 15 % of `MemTotal`). With the switch off, the nomination set is
byte-identical to before. **Metrics:** `engram_host_mem_free_pct` (gauge, per tick)
and `engram_host_idle_evict_kept_resident_total` (counter of `Soft` candidates kept
warm — the win signal; its rise should track a shift in the resume-latency
distribution toward the warm case).

**Why not just raise the TTL further.** A longer TTL still evicts on a fixed clock and
still can't tell "host is full, reclaim now" from "host is empty, keep it warm." The
pressure gate makes eviction track the resource it actually manages; the TTL stays the
*candidate* signal (how long since idle), pressure is the *reclaim* signal (do we need
the RAM). This is the ADR 0046 placement-reservation view (`allocatable_mib` already
counts guest residency) applied to the reclaim side. Scope note: still host-side +
env-gated; the coord state machine, the scanner, and the legality table are untouched.
